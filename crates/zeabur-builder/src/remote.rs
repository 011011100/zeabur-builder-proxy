//! Asks a Git server over HTTPS which commit a ref points at ([`zeabur_builder_core::git`]).
//!
//! Anonymous only: credentials for plain Git (plan §5.3) are not implemented yet. `ssh://` and
//! scp-like URLs are refused here; the Job could fetch them, but the controller cannot resolve
//! them without `ssh`.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{Method, Request, StatusCode, header};
use http_body_util::{BodyExt as _, Full, Limited};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use rustls::ClientConfig;
use thiserror::Error;
use zeabur_builder_core::git::{self, Advertisement, GitError, MAX_RESPONSE_BYTES};

const TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = concat!("git/zeabur-builder-", env!("CARGO_PKG_VERSION"));

/// Why a ref could not be resolved.
#[derive(Debug, Error)]
pub(crate) enum ResolveError {
    /// Not an `http://` or `https://` URL.
    #[error("only http and https repositories can be polled, not `{0}`")]
    Scheme(String),
    /// The server answered with an HTTP error.
    #[error("{0} answered {1}")]
    Status(String, StatusCode),
    /// The request failed or timed out.
    #[error("{0}: {1}")]
    Transport(String, String),
    /// The answer was not understood, or names no such ref.
    #[error(transparent)]
    Git(#[from] GitError),
}

impl ResolveError {
    /// The `SourceReady` reason.
    pub(crate) const fn reason(&self) -> &'static str {
        match self {
            Self::Git(GitError::RefNotFound(_)) => "RefNotFound",
            Self::Status(_, status)
                if status.as_u16() == 401 || status.as_u16() == 403 || status.as_u16() == 404 =>
            {
                "AuthFailed"
            }
            Self::Scheme(_) => "UnsupportedURL",
            Self::Status(..) | Self::Transport(..) | Self::Git(_) => "ResolveFailed",
        }
    }
}

/// An HTTP client for Git servers.
#[derive(Debug, Clone)]
pub(crate) struct Resolver {
    client: Client<HttpsConnector<HttpConnector>, Full<Bytes>>,
}

impl Resolver {
    /// Trusts the system's CA bundle (`SSL_CERT_FILE` or `/etc/ssl/certs`).
    pub(crate) fn new() -> Result<Self, String> {
        let mut roots = rustls::RootCertStore::empty();
        let found = rustls_native_certs::load_native_certs();
        for cert in found.certs {
            roots.add(cert).map_err(|e| e.to_string())?;
        }
        if roots.is_empty() {
            return Err(format!("no CA certificates found: {:?}", found.errors));
        }
        let config = ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
        let https = HttpsConnectorBuilder::new()
            .with_tls_config(config)
            .https_or_http()
            .enable_http1()
            .build();
        Ok(Self {
            client: Client::builder(TokioExecutor::new()).build(https),
        })
    }

    async fn request(
        &self,
        method: Method,
        url: &str,
        content_type: Option<&str>,
        body: Vec<u8>,
    ) -> Result<Bytes, ResolveError> {
        let transport =
            |e: &dyn std::fmt::Display| ResolveError::Transport(url.to_owned(), e.to_string());
        let mut request = Request::builder()
            .method(method)
            .uri(url)
            .header(header::USER_AGENT, USER_AGENT)
            .header("Git-Protocol", "version=2");
        if let Some(content_type) = content_type {
            request = request.header(header::CONTENT_TYPE, content_type);
        }
        let request = request
            .body(Full::new(Bytes::from(body)))
            .map_err(|e| transport(&e))?;
        let response = tokio::time::timeout(TIMEOUT, self.client.request(request))
            .await
            .map_err(|e| transport(&e))?
            .map_err(|e| transport(&e))?;
        if response.status() != StatusCode::OK {
            return Err(ResolveError::Status(url.to_owned(), response.status()));
        }
        let body = tokio::time::timeout(
            TIMEOUT,
            Limited::new(response.into_body(), MAX_RESPONSE_BYTES).collect(),
        )
        .await
        .map_err(|e| transport(&e))?
        .map_err(|e| transport(&*e))?;
        Ok(body.to_bytes())
    }

    /// The commit `git_ref` points at in the repository at `url`.
    pub(crate) async fn resolve(&self, url: &str, git_ref: &str) -> Result<String, ResolveError> {
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err(ResolveError::Scheme(url.to_owned()));
        }
        let base = url.trim_end_matches('/');
        let advertisement = self
            .request(
                Method::GET,
                &format!("{base}/info/refs?service=git-upload-pack"),
                None,
                Vec::new(),
            )
            .await?;
        let refs = match git::parse_advertisement(&advertisement)? {
            Advertisement::Refs(refs) => refs,
            Advertisement::V2 => {
                let answer = self
                    .request(
                        Method::POST,
                        &format!("{base}/git-upload-pack"),
                        Some("application/x-git-upload-pack-request"),
                        git::ls_refs_request(git_ref),
                    )
                    .await?;
                git::parse_ls_refs(&answer)?
            }
        };
        Ok(git::resolve(git_ref, &refs)?)
    }
}
