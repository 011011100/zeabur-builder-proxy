//! Pulls an archive source from an OCI registry ([distribution spec], "Pulling manifests" and
//! "Pulling blobs").
//!
//! - **Credentials.** The `kubernetes.io/dockerconfigjson` Secret of the namespace, mounted at
//!   `$DOCKER_CONFIG/config.json`. They go to the registry as basic auth. If the registry asks for
//!   a bearer token (`WWW-Authenticate: Bearer`), the same credentials buy a pull token from its
//!   realm ([token authentication]).
//! - **Trust.** The system's CAs and the PEM in `ZB_REGISTRY_CA`, the CA buildkitd trusts.
//! - **Checks.** The manifest must hash to the run's digest and the layer to its own digest and
//!   size. The layer goes to a file first, so nothing unverified is unpacked.
//! - **Redirects.** Blob downloads may redirect to storage, such as S3. Credentials never go to
//!   another host, and `https` never becomes `http`.
//!
//! [distribution spec]: https://github.com/opencontainers/distribution-spec/blob/v1.1.1/spec.md#pull
//! [token authentication]: https://distribution.github.io/distribution/spec/auth/token/

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use http::{HeaderValue, Request, Response, StatusCode, header};
use http_body_util::{BodyExt as _, Empty};
use hyper::body::Incoming;
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject as _;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use zeabur_builder_core::archive::{self, Layer, MANIFEST_MEDIA_TYPE, MAX_MANIFEST_BYTES};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REDIRECTS: usize = 5;
const MAX_TOKEN_BYTES: u64 = 1 << 20;
const USER_AGENT: &str = concat!("zeabur-builder-job/", env!("CARGO_PKG_VERSION"));

type HttpClient = Client<HttpsConnector<HttpConnector>, Empty<Bytes>>;

/// A client that trusts the system's CAs and `ca`, a PEM bundle.
pub(crate) fn client(ca: Option<&str>) -> Result<HttpClient, String> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        // A system certificate rustls cannot parse is skipped, as the controller does not need it.
        drop(roots.add(cert));
    }
    if let Some(ca) = ca {
        for cert in CertificateDer::pem_slice_iter(ca.as_bytes()) {
            let cert = cert.map_err(|e| format!("cannot read the registry CA: {e}"))?;
            roots
                .add(cert)
                .map_err(|e| format!("cannot use the registry CA: {e}"))?;
        }
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| e.to_string())?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_connect_timeout(Some(CONNECT_TIMEOUT));
    let https = HttpsConnectorBuilder::new()
        .with_tls_config(config)
        .https_or_http()
        .enable_http1()
        .wrap_connector(http);
    Ok(Client::builder(TokioExecutor::new()).build(https))
}

#[derive(Deserialize)]
struct DockerConfig {
    #[serde(default)]
    auths: BTreeMap<String, AuthEntry>,
}

#[derive(Deserialize)]
struct AuthEntry {
    #[serde(default)]
    auth: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
}

/// A token endpoint's answer. Docker's registry writes `token`, OAuth servers `access_token`.
#[derive(Deserialize)]
struct Token {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
}

/// The `Authorization` value for `host` in a Docker config: its `auth`, or `username` and
/// `password`. A key may carry a scheme and a path, as `docker login` writes for Docker Hub.
pub(crate) fn basic_auth(config: &[u8], host: &str) -> Result<Option<String>, String> {
    let config: DockerConfig = serde_json::from_slice(config)
        .map_err(|e| format!("cannot read the Docker config: {e}"))?;
    let entry = config.auths.into_iter().find_map(|(key, entry)| {
        let key = key
            .strip_prefix("https://")
            .or_else(|| key.strip_prefix("http://"))
            .unwrap_or(&key);
        (key.split('/').next() == Some(host)).then_some(entry)
    });
    Ok(entry.and_then(|entry| {
        let pair = entry.auth.or_else(|| {
            let pair = format!("{}:{}", entry.username?, entry.password?);
            Some(STANDARD.encode(pair))
        })?;
        Some(format!("Basic {pair}"))
    }))
}

/// A bearer challenge.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Challenge {
    realm: String,
    service: Option<String>,
}

/// Reads `Bearer realm="…",service="…"` from a `WWW-Authenticate` header ([RFC 6750] §3).
///
/// [RFC 6750]: https://www.rfc-editor.org/rfc/rfc6750#section-3
pub(crate) fn bearer_challenge(value: &str) -> Option<Challenge> {
    let (scheme, rest) = value.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let mut params = BTreeMap::new();
    let mut rest = rest.trim_start();
    while !rest.is_empty() {
        let (key, after) = rest.split_once('=')?;
        let (value, after) = if let Some(quoted) = after.strip_prefix('"') {
            let end = quoted.find('"')?;
            (quoted.get(..end)?, quoted.get(end + 1..)?)
        } else {
            let end = after.find(',').unwrap_or(after.len());
            (after.get(..end)?, after.get(end..)?)
        };
        params.insert(key.trim().to_ascii_lowercase(), value.to_owned());
        rest = after.trim_start_matches([',', ' ']);
    }
    Some(Challenge {
        realm: params.remove("realm")?,
        service: params.remove("service"),
    })
}

/// Percent-encodes a query value: everything except the unreserved characters of RFC 3986.
fn encode(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push('%');
            for nibble in [b >> 4, b & 0xF] {
                let digit = char::from_digit(u32::from(nibble), 16).unwrap_or('0');
                out.push(digit.to_ascii_uppercase());
            }
        }
    }
    out
}

/// One repository of one registry.
pub(crate) struct Repository {
    client: HttpClient,
    scheme: &'static str,
    host: String,
    name: String,
    basic: Option<String>,
    bearer: Option<String>,
}

impl std::fmt::Debug for Repository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Repository")
            .field("host", &self.host)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl Repository {
    /// `repository` is `<host>/<name>`.
    pub(crate) fn new(
        client: HttpClient,
        repository: &str,
        plain_http: bool,
        basic: Option<String>,
    ) -> Result<Self, String> {
        let (host, name) = repository
            .split_once('/')
            .filter(|(host, name)| !host.is_empty() && !name.is_empty())
            .ok_or_else(|| format!("`{repository}` is not <host>/<name>"))?;
        Ok(Self {
            client,
            scheme: if plain_http { "http" } else { "https" },
            host: host.to_owned(),
            name: name.to_owned(),
            basic,
            bearer: None,
        })
    }

    async fn send(
        &self,
        url: &str,
        accept: Option<&str>,
        authorization: Option<&str>,
    ) -> Result<Response<Incoming>, String> {
        let mut request = Request::get(url).header(header::USER_AGENT, USER_AGENT);
        if let Some(accept) = accept {
            request = request.header(header::ACCEPT, accept);
        }
        if let Some(value) = authorization {
            let mut value =
                HeaderValue::from_str(value).map_err(|_e| "invalid credentials".to_owned())?;
            value.set_sensitive(true);
            request = request.header(header::AUTHORIZATION, value);
        }
        let request = request
            .body(Empty::new())
            .map_err(|e| format!("{url}: {e}"))?;
        self.client
            .request(request)
            .await
            .map_err(|e| format!("{url}: {e:?}"))
    }

    /// GETs `/v2/<name>/<path>`, with a pull token if the registry asks for one, following
    /// redirects.
    async fn get(
        &mut self,
        path: &str,
        accept: Option<&str>,
    ) -> Result<Response<Incoming>, String> {
        let first = format!("{}://{}/v2/{}/{path}", self.scheme, self.host, self.name);
        let mut url = first.clone();
        let mut same_host = true;
        for _ in 0..=MAX_REDIRECTS {
            let authorization = if same_host {
                self.bearer.as_deref().or(self.basic.as_deref())
            } else {
                None
            };
            let response = self.send(&url, accept, authorization).await?;
            let status = response.status();
            if status == StatusCode::UNAUTHORIZED && same_host && self.bearer.is_none() {
                let challenge = response
                    .headers()
                    .get(header::WWW_AUTHENTICATE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(bearer_challenge);
                if let Some(challenge) = challenge {
                    self.bearer = Some(self.token(&challenge).await?);
                    continue;
                }
            }
            if status.is_redirection() {
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| format!("{url} answered {status} without a Location"))?;
                let next = if location.starts_with('/') {
                    format!("{}://{}{location}", self.scheme, self.host)
                } else {
                    location.to_owned()
                };
                let next_uri: http::Uri = next
                    .parse()
                    .map_err(|e| format!("redirect to {next}: {e}"))?;
                if self.scheme == "https" && next_uri.scheme_str() != Some("https") {
                    return Err(format!("{url} redirects to {next}, which is not https"));
                }
                same_host &= next_uri.authority().map(http::uri::Authority::as_str)
                    == Some(self.host.as_str());
                url = next;
                continue;
            }
            if !status.is_success() {
                return Err(format!("{first} answered {status}"));
            }
            return Ok(response);
        }
        Err(format!("{first}: too many redirects"))
    }

    /// A pull token from the challenge's realm.
    async fn token(&self, challenge: &Challenge) -> Result<String, String> {
        let service = challenge
            .service
            .as_deref()
            .map_or_else(String::new, |s| format!("&service={}", encode(s)));
        let url = format!(
            "{}{}scope={}{service}",
            challenge.realm,
            if challenge.realm.contains('?') {
                '&'
            } else {
                '?'
            },
            encode(&format!("repository:{}:pull", self.name))
        );
        let response = self.send(&url, None, self.basic.as_deref()).await?;
        if !response.status().is_success() {
            return Err(format!(
                "{} answered {}",
                challenge.realm,
                response.status()
            ));
        }
        let body = read_limited(response, MAX_TOKEN_BYTES).await?;
        let token: Token =
            serde_json::from_slice(&body).map_err(|e| format!("cannot read the token: {e}"))?;
        token
            .token
            .or(token.access_token)
            .map(|t| format!("Bearer {t}"))
            .ok_or_else(|| format!("{} gave no token", challenge.realm))
    }

    /// The archive's layer, from the manifest with `digest`.
    pub(crate) async fn layer(&mut self, digest: &str) -> Result<Layer, String> {
        let response = self
            .get(&format!("manifests/{digest}"), Some(MANIFEST_MEDIA_TYPE))
            .await?;
        let body = read_limited(response, MAX_MANIFEST_BYTES).await?;
        archive::layer(&body, digest).map_err(|e| e.to_string())
    }

    /// Downloads the layer to `to`, checks its size and digest, and removes the file if it fails.
    pub(crate) async fn download(&mut self, layer: &Layer, to: &Path) -> Result<(), String> {
        let written = self.download_unchecked(layer, to).await;
        if written.is_err() {
            drop(std::fs::remove_file(to));
        }
        written
    }

    async fn download_unchecked(&mut self, layer: &Layer, to: &Path) -> Result<(), String> {
        let mut response = self.get(&format!("blobs/{}", layer.digest), None).await?;
        let mut file = std::fs::File::create_new(to)
            .map_err(|e| format!("cannot create {}: {e}", to.display()))?;
        let mut hash = Sha256::new();
        let mut size = 0_u64;
        while let Some(frame) = response.body_mut().frame().await {
            let frame = frame.map_err(|e| format!("cannot download the archive: {e}"))?;
            let Ok(data) = frame.into_data() else {
                continue;
            };
            size += u64::try_from(data.len()).unwrap_or(u64::MAX);
            if size > layer.size {
                return Err(format!(
                    "the archive is larger than the {} bytes its manifest gives",
                    layer.size
                ));
            }
            hash.update(&data);
            file.write_all(&data)
                .map_err(|e| format!("cannot write {}: {e}", to.display()))?;
        }
        if size != layer.size {
            return Err(format!(
                "the archive has {size} bytes, not the {} its manifest gives",
                layer.size
            ));
        }
        if archive::hex(&hash.finalize()) != layer.digest {
            return Err(format!("the archive does not match {}", layer.digest));
        }
        Ok(())
    }
}

/// The body, refused when it is longer than `limit`.
async fn read_limited(response: Response<Incoming>, limit: u64) -> Result<Bytes, String> {
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    http_body_util::Limited::new(response.into_body(), limit)
        .collect()
        .await
        .map(http_body_util::Collected::to_bytes)
        .map_err(|e| format!("cannot read the answer: {e}"))
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::net::SocketAddr;

    use http_body_util::Full;
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    use zeabur_builder_core::archive::{
        ARTIFACT_TYPE, EMPTY_CONFIG_MEDIA_TYPE, LAYER_MEDIA_TYPE, sha256,
    };

    use super::*;

    const NAME: &str = "team-a/api/source";
    const BASIC: &str = "Basic dGVhbS1hOnNlY3JldA==";

    #[test]
    fn credentials_for_the_registry_host() {
        let config = br#"{"auths":{
            "https://index.docker.io/v1/":{"auth":"aHViOmh1Yg=="},
            "zot.zot.svc:5000":{"username":"team-a","password":"secret"}}}"#;
        assert_eq!(
            basic_auth(config, "zot.zot.svc:5000").unwrap().as_deref(),
            Some(BASIC)
        );
        assert_eq!(
            basic_auth(config, "index.docker.io").unwrap().as_deref(),
            Some("Basic aHViOmh1Yg==")
        );
        assert_eq!(basic_auth(config, "zot.zot.svc").unwrap(), None);
        assert_eq!(basic_auth(br#"{"auths":{}}"#, "zot").unwrap(), None);
    }

    #[test]
    fn bearer_challenges() {
        assert_eq!(
            bearer_challenge(
                r#"Bearer realm="https://auth.example/token?a=1,b",service="registry.example",scope="repository:x:pull""#
            ),
            Some(Challenge {
                realm: "https://auth.example/token?a=1,b".to_owned(),
                service: Some("registry.example".to_owned()),
            })
        );
        assert_eq!(
            bearer_challenge("bearer realm=https://auth.example/token"),
            Some(Challenge {
                realm: "https://auth.example/token".to_owned(),
                service: None,
            })
        );
        assert_eq!(bearer_challenge(r#"Basic realm="zot""#), None);
        assert_eq!(bearer_challenge(r#"Bearer service="x""#), None);
    }

    struct Fake {
        manifest: Vec<u8>,
        blob: Vec<u8>,
        addr: SocketAddr,
    }

    fn reply(
        status: StatusCode,
        headers: &[(&str, String)],
        body: Vec<u8>,
    ) -> Response<Full<Bytes>> {
        let mut response = Response::builder().status(status);
        for (name, value) in headers {
            response = response.header(*name, value);
        }
        response.body(Full::new(Bytes::from(body))).unwrap()
    }

    /// A registry that wants a bearer token for `team-a/api/source`, sold for [`BASIC`], and that
    /// redirects blobs to storage on another host name, which refuses credentials.
    fn handle(fake: &Fake, request: &Request<Incoming>) -> Response<Full<Bytes>> {
        let auth = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        let path = request.uri().path();
        let query = request.uri().query().unwrap_or_default();
        let port = fake.addr.port();
        if path == "/token" {
            let scope = format!("scope={}", encode(&format!("repository:{NAME}:pull")));
            return if auth == Some(BASIC)
                && query.contains(&scope)
                && query.contains("service=fake")
            {
                reply(StatusCode::OK, &[], br#"{"token":"t0k"}"#.to_vec())
            } else {
                reply(StatusCode::FORBIDDEN, &[], Vec::new())
            };
        }
        if path == "/storage/blob" {
            return if auth.is_none() {
                reply(StatusCode::OK, &[], fake.blob.clone())
            } else {
                reply(StatusCode::BAD_REQUEST, &[], Vec::new())
            };
        }
        if auth != Some("Bearer t0k") {
            let challenge =
                format!(r#"Bearer realm="http://127.0.0.1:{port}/token",service="fake""#);
            return reply(
                StatusCode::UNAUTHORIZED,
                &[("www-authenticate", challenge)],
                Vec::new(),
            );
        }
        let Some(rest) = path.strip_prefix(&format!("/v2/{NAME}/")) else {
            return reply(StatusCode::NOT_FOUND, &[], Vec::new());
        };
        if rest.starts_with("manifests/") {
            reply(StatusCode::OK, &[], fake.manifest.clone())
        } else if rest.starts_with("blobs/") {
            let location = format!("http://localhost:{port}/storage/blob");
            reply(
                StatusCode::TEMPORARY_REDIRECT,
                &[("location", location)],
                Vec::new(),
            )
        } else {
            reply(StatusCode::NOT_FOUND, &[], Vec::new())
        }
    }

    async fn serve(manifest: Vec<u8>, blob: Vec<u8>) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let fake = Arc::new(Fake {
            manifest,
            blob,
            addr,
        });
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let fake = Arc::clone(&fake);
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        let response = handle(&fake, &request);
                        async move { Ok::<_, Infallible>(response) }
                    });
                    drop(
                        hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await,
                    );
                });
            }
        });
        addr
    }

    fn manifest(blob: &[u8]) -> Vec<u8> {
        format!(
            r#"{{"schemaVersion":2,"mediaType":"{MANIFEST_MEDIA_TYPE}","artifactType":"{ARTIFACT_TYPE}",
            "config":{{"mediaType":"{EMPTY_CONFIG_MEDIA_TYPE}","digest":"{}","size":2}},
            "layers":[{{"mediaType":"{LAYER_MEDIA_TYPE}","digest":"{}","size":{}}}]}}"#,
            sha256(b"{}"),
            sha256(blob),
            blob.len()
        )
        .into_bytes()
    }

    async fn pull(addr: SocketAddr, digest: &str, to: &Path) -> Result<(), String> {
        let mut repository = Repository::new(
            client(None).unwrap(),
            &format!("127.0.0.1:{}/{NAME}", addr.port()),
            true,
            Some(BASIC.to_owned()),
        )
        .unwrap();
        let layer = repository.layer(digest).await?;
        repository.download(&layer, to).await
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "zeabur-builder-registry-{}-{name}",
            std::process::id()
        ));
        drop(std::fs::remove_file(&path));
        path
    }

    #[tokio::test]
    async fn pulls_with_a_token_and_follows_redirects_without_credentials() {
        let blob = b"the archive".to_vec();
        let manifest = manifest(&blob);
        let digest = sha256(&manifest);
        let addr = serve(manifest, blob.clone()).await;
        let to = scratch("ok");
        pull(addr, &digest, &to).await.unwrap();
        assert_eq!(std::fs::read(&to).unwrap(), blob);
        std::fs::remove_file(&to).unwrap();
    }

    #[tokio::test]
    async fn refuses_content_that_does_not_match() {
        let blob = b"the archive".to_vec();
        let manifest = manifest(&blob);
        let digest = sha256(&manifest);

        let swapped = serve(manifest.clone(), b"the ARCHIVE".to_vec()).await;
        let to = scratch("swapped");
        assert!(pull(swapped, &digest, &to).await.is_err());
        assert!(!to.exists());

        let longer = serve(manifest.clone(), b"the archive and more".to_vec()).await;
        assert!(pull(longer, &digest, &to).await.is_err());
        assert!(!to.exists());

        let honest = serve(manifest, blob).await;
        let other = sha256(b"another manifest");
        assert!(pull(honest, &other, &to).await.is_err());
        assert!(!to.exists());
    }
}
