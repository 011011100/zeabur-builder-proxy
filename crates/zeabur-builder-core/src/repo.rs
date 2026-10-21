//! Repository keys.
//!
//! The same repository arrives as `https://GitHub.com/Acme/api.git` in a `Build`,
//! `git@github.com:acme/api.git` in another, and `https://github.com/acme/api` in a webhook. The
//! controller indexes `Build`s by [`RepoKey`] and groups polls by it, so all three must give one
//! key. The rules:
//!
//! - The scheme, user name and default port (22, 80, 443, 9418) are dropped: the key names a
//!   repository, not a way to reach it. Other ports stay, since they can be another server.
//! - The scp-like SSH form `user@host:path` becomes `host/path`, as Git itself reads it
//!   ([git-clone], "GIT URLS").
//! - The host is lowercased (DNS names are case-insensitive), and a trailing `/` or `.git` is
//!   dropped.
//! - On github.com, owner and repository names are case-insensitive, so the path is lowercased too.
//!   Other servers keep the path's case, because some (Gitea on a case-sensitive filesystem, for
//!   one) treat it as significant. Not verified for GitLab, which redirects mixed case.
//!
//! [git-clone]: https://git-scm.com/docs/git-clone#_git_urls

use std::fmt;

use thiserror::Error;

/// Hosts whose repository paths are case-insensitive.
const CASE_INSENSITIVE_HOSTS: &[&str] = &["github.com"];

/// A canonical repository key, `host[:port]/path`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoKey(String);

/// Why a URL is not a Git repository URL.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RepoUrlError {
    /// The scheme is not one Git fetches over the network.
    #[error("unsupported URL scheme `{0}`: use https, http, ssh or git")]
    Scheme(String),
    /// No host, or no repository path.
    #[error("the URL needs a host and a repository path")]
    Incomplete,
    /// Characters that never appear in a host name.
    #[error("invalid host `{0}`")]
    Host(String),
}

impl RepoKey {
    /// Parses and normalises a repository URL.
    ///
    /// # Errors
    ///
    /// When the URL is a local path, uses another scheme, or lacks a host or path.
    pub fn parse(url: &str) -> Result<Self, RepoUrlError> {
        let url = url.trim();
        let (scheme, rest) = match url.split_once("://") {
            Some((scheme, rest)) => (scheme.to_ascii_lowercase(), rest.to_owned()),
            None => scp_like(url)?,
        };
        let default_port = match scheme.as_str() {
            "https" => "443",
            "http" => "80",
            "ssh" | "git+ssh" | "ssh+git" => "22",
            "git" => "9418",
            other => return Err(RepoUrlError::Scheme(other.to_owned())),
        };
        let (authority, path) = rest.split_once('/').ok_or(RepoUrlError::Incomplete)?;
        let host_port = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        let host_port = host_port.to_ascii_lowercase();
        let host_port = match host_port.rsplit_once(':') {
            Some((host, port)) if port == default_port => host.to_owned(),
            Some((_, "")) => host_port.trim_end_matches(':').to_owned(),
            Some(_) | None => host_port,
        };
        let host = host_port.split(':').next().unwrap_or_default();
        if host.is_empty() {
            return Err(RepoUrlError::Incomplete);
        }
        if !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        {
            return Err(RepoUrlError::Host(host.to_owned()));
        }
        let path = path.split(['?', '#']).next().unwrap_or_default();
        let path = path.trim_matches('/');
        let path = path
            .strip_suffix(".git")
            .unwrap_or(path)
            .trim_end_matches('/');
        if path.is_empty() {
            return Err(RepoUrlError::Incomplete);
        }
        let path = if CASE_INSENSITIVE_HOSTS.contains(&host) {
            path.to_ascii_lowercase()
        } else {
            path.to_owned()
        };
        Ok(Self(format!("{host_port}/{path}")))
    }

    /// The key as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The host, without a port.
    #[must_use]
    pub fn host(&self) -> &str {
        let authority = self.0.split('/').next().unwrap_or_default();
        authority.split(':').next().unwrap_or_default()
    }

    /// The key of one ref in this repository, `<repo>#<ref>`, as the `Build` index stores it.
    #[must_use]
    pub fn with_ref(&self, git_ref: &str) -> String {
        let git_ref = git_ref.strip_prefix("refs/heads/").unwrap_or(git_ref);
        format!("{}#{git_ref}", self.0)
    }
}

impl fmt::Display for RepoKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// `user@host:path` → `("ssh", "user@host/path")`. Git reads it so only when no slash comes
/// before the first colon; otherwise it is a local path.
fn scp_like(url: &str) -> Result<(String, String), RepoUrlError> {
    let (authority, path) = url
        .split_once(':')
        .ok_or_else(|| RepoUrlError::Scheme(String::new()))?;
    // `ext::cmd` and `fd::0` are remote helpers, not hosts.
    if authority.contains('/') || path.starts_with(':') {
        return Err(RepoUrlError::Scheme(authority.to_owned()));
    }
    Ok(("ssh".to_owned(), format!("{authority}/{path}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spellings_of_one_repository() {
        for url in [
            "https://GitHub.com/Acme/api.git",
            "https://github.com/acme/api",
            "https://github.com/acme/api/",
            "https://github.com:443/acme/api.git",
            "http://github.com/acme/api",
            "git@github.com:acme/api.git",
            "git@github.com:Acme/API",
            "ssh://git@github.com/acme/api",
            "ssh://git@github.com:22/acme/api.git",
            "git://github.com/acme/api.git",
            "https://token@github.com/acme/api.git",
        ] {
            assert_eq!(
                RepoKey::parse(url).unwrap().as_str(),
                "github.com/acme/api",
                "{url}"
            );
        }
    }

    #[test]
    fn other_hosts_keep_path_case_and_ports() {
        assert_eq!(
            RepoKey::parse("https://Gitea.Example.com/Team/App.git")
                .unwrap()
                .as_str(),
            "gitea.example.com/Team/App"
        );
        assert_eq!(
            RepoKey::parse("ssh://git@git.example.com:2222/team/app")
                .unwrap()
                .as_str(),
            "git.example.com:2222/team/app"
        );
        assert_eq!(
            RepoKey::parse("git@git.example.com:group/sub/app.git")
                .unwrap()
                .as_str(),
            "git.example.com/group/sub/app"
        );
    }

    #[test]
    fn rejects_local_paths_and_other_schemes() {
        for url in [
            "/srv/git/app.git",
            "./app",
            "file:///srv/git/app",
            "https://github.com/",
            "https:///acme/api",
            "ftp://x.y/z",
            "ext::sh -c touch% /tmp/x",
            "fd::0",
        ] {
            assert!(RepoKey::parse(url).is_err(), "{url}");
        }
    }

    #[test]
    fn ref_keys() {
        let key = RepoKey::parse("git@github.com:acme/api.git").unwrap();
        assert_eq!(key.with_ref("refs/heads/main"), "github.com/acme/api#main");
        assert_eq!(key.with_ref("v1.2.0"), "github.com/acme/api#v1.2.0");
        assert_eq!(key.host(), "github.com");
    }
}
