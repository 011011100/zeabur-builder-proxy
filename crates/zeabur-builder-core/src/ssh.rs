//! SSH remotes: where an `ssh://` or scp-like URL points, and which `known_hosts` keys vouch for
//! that host.
//!
//! `known_hosts` is read as `ssh` reads it ([sshd(8)], "`SSH_KNOWN_HOSTS` FILE FORMAT"), without
//! hashed host names, wildcards or markers (`@cert-authority`, `@revoked`): lines using them never
//! match, so a host they would cover is unknown rather than trusted.
//!
//! [sshd(8)]: https://man.openbsd.org/sshd.8#SSH_KNOWN_HOSTS_FILE_FORMAT

/// The user when the URL names none. Git hosts take any key as `git`.
const DEFAULT_USER: &str = "git";

/// A repository reached over SSH.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshRemote {
    /// The login name.
    pub user: String,
    /// The host name or address, lowercased, without brackets.
    pub host: String,
    /// The port.
    pub port: u16,
    /// The repository path as the server is asked for it.
    pub path: String,
}

impl SshRemote {
    /// Reads `ssh://[user@]host[:port]/path` or the scp-like `[user@]host:path`; `None` for any
    /// other URL.
    #[must_use]
    pub fn parse(url: &str) -> Option<Self> {
        let url = url.trim();
        if let Some((scheme, rest)) = url.split_once("://") {
            if !matches!(
                scheme.to_ascii_lowercase().as_str(),
                "ssh" | "git+ssh" | "ssh+git"
            ) {
                return None;
            }
            let (authority, path) = rest.split_once('/')?;
            let (user, host_port) = split_user(authority);
            let (host, port) = match host_port.strip_prefix('[') {
                Some(bracketed) => {
                    let (host, after) = bracketed.split_once(']')?;
                    match after {
                        "" => (host, None),
                        _ => (host, Some(after.strip_prefix(':')?)),
                    }
                }
                None => match host_port.split_once(':') {
                    Some((host, port)) => (host, Some(port)),
                    None => (host_port, None),
                },
            };
            let port = match port {
                None | Some("") => 22,
                Some(port) => port.parse().ok().filter(|&p| p != 0)?,
            };
            // Git drops the slash before `~user/…`.
            let path = if path.starts_with('~') {
                path.to_owned()
            } else {
                format!("/{path}")
            };
            Self::new(user, host, port, path)
        } else {
            let (authority, path) = url.split_once(':')?;
            // As in `repo`: a slash before the colon makes it a local path.
            if authority.contains('/') || path.starts_with(':') {
                return None;
            }
            let (user, host) = split_user(authority);
            Self::new(user, host, 22, path.to_owned())
        }
    }

    fn new(user: Option<&str>, host: &str, port: u16, path: String) -> Option<Self> {
        let valid_host = !host.is_empty()
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':'));
        if !valid_host || path.trim_matches('/').is_empty() {
            return None;
        }
        Some(Self {
            user: user.unwrap_or(DEFAULT_USER).to_owned(),
            host: host.to_ascii_lowercase(),
            port,
            path,
        })
    }

    /// The command run on the server: `git-upload-pack '<path>'`, quoted as Git quotes it.
    #[must_use]
    pub fn upload_pack_command(&self) -> String {
        let mut quoted = String::from("'");
        for c in self.path.chars() {
            match c {
                '\'' => quoted.push_str("'\\''"),
                '!' => quoted.push_str("'\\!'"),
                c => quoted.push(c),
            }
        }
        quoted.push('\'');
        format!("git-upload-pack {quoted}")
    }
}

fn split_user(authority: &str) -> (Option<&str>, &str) {
    match authority.rsplit_once('@') {
        Some((user, host)) if !user.is_empty() => (Some(user), host),
        Some((_, host)) => (None, host),
        None => (None, authority),
    }
}

/// One key from `known_hosts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnownKey<'a> {
    /// The key type, such as `ssh-ed25519`.
    pub algorithm: &'a str,
    /// The key, base64.
    pub key: &'a str,
}

/// The keys `known_hosts` lists for `host` on `port`.
///
/// On port 22 the host is written plainly or as `[host]:22`; on any other port only as
/// `[host]:port`. A negated pattern (`!host`) that matches excludes the whole line.
#[must_use]
pub fn known_keys<'a>(known_hosts: &'a str, host: &str, port: u16) -> Vec<KnownKey<'a>> {
    let bracketed = format!("[{host}]:{port}");
    let matches = |pattern: &str| {
        pattern.eq_ignore_ascii_case(&bracketed)
            || (port == 22 && pattern.eq_ignore_ascii_case(host))
    };
    known_hosts
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with('@') {
                return None;
            }
            let mut fields = line.split_ascii_whitespace();
            let (patterns, algorithm, key) = (fields.next()?, fields.next()?, fields.next()?);
            let mut hit = false;
            for pattern in patterns.split(',') {
                match pattern.strip_prefix('!') {
                    Some(negated) if matches(negated) => return None,
                    Some(_) => {}
                    None => hit |= matches(pattern),
                }
            }
            hit.then_some(KnownKey { algorithm, key })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(user: &str, host: &str, port: u16, path: &str) -> SshRemote {
        SshRemote {
            user: user.to_owned(),
            host: host.to_owned(),
            port,
            path: path.to_owned(),
        }
    }

    #[test]
    fn urls() {
        let cases = [
            (
                "git@github.com:acme/api.git",
                Some(remote("git", "github.com", 22, "acme/api.git")),
            ),
            (
                "GitHub.com:acme/api.git",
                Some(remote("git", "github.com", 22, "acme/api.git")),
            ),
            (
                "ssh://git@github.com/acme/api.git",
                Some(remote("git", "github.com", 22, "/acme/api.git")),
            ),
            (
                "ssh://deploy@git.example.com:2222/srv/api.git",
                Some(remote("deploy", "git.example.com", 2222, "/srv/api.git")),
            ),
            (
                "git+ssh://git.example.com/~alice/api.git",
                Some(remote("git", "git.example.com", 22, "~alice/api.git")),
            ),
            (
                "ssh://git@[2001:db8::1]:2222/api.git",
                Some(remote("git", "2001:db8::1", 2222, "/api.git")),
            ),
            ("https://github.com/acme/api.git", None),
            ("git://github.com/acme/api.git", None),
            ("./local:path", None),
            ("ext::sh -c id", None),
            ("ssh://git@github.com", None),
            ("ssh://git@github.com/", None),
            ("ssh://git@github.com:0/acme/api.git", None),
            ("ssh://git@github.com:x/acme/api.git", None),
            ("ssh://git@exa$mple.com/acme/api.git", None),
            ("git@github.com:", None),
        ];
        for (url, expected) in cases {
            assert_eq!(SshRemote::parse(url), expected, "{url}");
        }
    }

    #[test]
    fn upload_pack_command() {
        let quote = |path: &str| {
            SshRemote::parse(&format!("git@example.com:{path}"))
                .map_or_else(String::new, |r| r.upload_pack_command())
        };
        assert_eq!(quote("acme/api.git"), "git-upload-pack 'acme/api.git'");
        assert_eq!(quote("it's!.git"), r"git-upload-pack 'it'\''s'\!'.git'");
    }

    const KNOWN_HOSTS: &str = "\
# comment
github.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl
github.com,gist.github.com ecdsa-sha2-nistp256 AAAAE2VjZHNh github
[git.example.com]:2222 ssh-ed25519 AAAAport
git.example.com ssh-ed25519 AAAAplain
*.example.org ssh-ed25519 AAAAwild
|1|c2FsdA==|aGFzaA== ssh-ed25519 AAAAhashed
@revoked github.com ssh-ed25519 AAAArevoked
gitlab.com,!gitlab.com ssh-ed25519 AAAAnegated
truncated.example.com ssh-ed25519
";

    fn keys(host: &str, port: u16) -> Vec<&'static str> {
        known_keys(KNOWN_HOSTS, host, port)
            .iter()
            .map(|k| k.key)
            .collect()
    }

    #[test]
    fn known_hosts() {
        assert_eq!(
            known_keys(KNOWN_HOSTS, "github.com", 22),
            vec![
                KnownKey {
                    algorithm: "ssh-ed25519",
                    key: "AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl",
                },
                KnownKey {
                    algorithm: "ecdsa-sha2-nistp256",
                    key: "AAAAE2VjZHNh",
                },
            ]
        );
        assert_eq!(keys("GITHUB.COM", 22), keys("github.com", 22));
        assert_eq!(keys("gist.github.com", 22), vec!["AAAAE2VjZHNh"]);
        assert_eq!(keys("github.com", 2222), Vec::<&str>::new());
        assert_eq!(keys("git.example.com", 2222), vec!["AAAAport"]);
        assert_eq!(keys("git.example.com", 22), vec!["AAAAplain"]);
        assert_eq!(keys("a.example.org", 22), Vec::<&str>::new());
        assert_eq!(keys("gitlab.com", 22), Vec::<&str>::new());
        assert_eq!(keys("truncated.example.com", 22), Vec::<&str>::new());
        assert_eq!(keys("example.com", 22), Vec::<&str>::new());
    }
}
