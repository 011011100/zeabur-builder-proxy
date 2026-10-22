//! Posts a notification (plan §9) with retries.
//!
//! Basic auth is what a `keel` target's `secretRef` gives: Keel checks it on its webhooks when
//! `BASIC_AUTH_USER` and `BASIC_AUTH_PASSWORD` are set (not verified).

use std::fmt;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use http::{Method, Request, header};
use http_body_util::Full;

use crate::decide::NOTIFY_BACKOFF;
use crate::remote::HttpClient;

const TIMEOUT: Duration = Duration::from_secs(10);
const USER_AGENT: &str = concat!("zeabur-builder/", env!("CARGO_PKG_VERSION"));

/// HTTP basic auth.
pub(crate) struct Credentials {
    pub(crate) username: String,
    pub(crate) password: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

/// POSTs `body` as JSON to `url`, at most three times; the last error when none got a 2xx.
pub(crate) async fn post(
    client: &HttpClient,
    url: &str,
    credentials: Option<&Credentials>,
    body: &[u8],
) -> Result<(), String> {
    let mut waits = NOTIFY_BACKOFF.iter();
    loop {
        let error = match attempt(client, url, credentials, body).await {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        let Some(wait) = waits.next() else {
            return Err(error);
        };
        tracing::info!(url, error, "notify failed; retrying");
        tokio::time::sleep(*wait).await;
    }
}

async fn attempt(
    client: &HttpClient,
    url: &str,
    credentials: Option<&Credentials>,
    body: &[u8],
) -> Result<(), String> {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri(url)
        .header(header::USER_AGENT, USER_AGENT)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(c) = credentials {
        let token = STANDARD.encode(format!("{}:{}", c.username, c.password));
        request = request.header(header::AUTHORIZATION, format!("Basic {token}"));
    }
    let request = request
        .body(Full::new(Bytes::copy_from_slice(body)))
        .map_err(|e| e.to_string())?;
    let response = tokio::time::timeout(TIMEOUT, client.request(request))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!("answered {}", response.status()))
    }
}
