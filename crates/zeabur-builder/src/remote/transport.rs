//! HTTP proxies below TLS: CONNECT negotiates a byte tunnel, then hyper-rustls verifies the
//! original destination using the unchanged CA roots and server name. Plain HTTP uses
//! [absolute-form] request targets. A successful [CONNECT] response ends at the header boundary;
//! no tunneled bytes are read ahead or discarded. Proxy responses are never logged.
//!
//! [absolute-form]: https://www.rfc-editor.org/rfc/rfc9112.html#section-3.2.2
//! [CONNECT]: https://www.rfc-editor.org/rfc/rfc9110.html#section-9.3.6

use std::future::Future;
use std::io;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use http::Uri;
use hyper::rt::{Read, ReadBufCursor, Write};
use hyper_util::client::legacy::connect::{Connected, Connection, HttpConnector};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tower_service::Service;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONNECT_HEADERS: usize = 16 * 1024;
// These are the installer's cluster and VPC ranges. Keep this list in sync with the chart.
const INTERNAL_HOSTS: &str = "localhost,127.0.0.1,::1,.svc,.svc.cluster.local,10.43.0.1,10.42.0.0/16,10.43.0.0/16,172.16.0.0/12";
static CONFIG: OnceLock<Result<Arc<ProxyConfig>, String>> = OnceLock::new();

fn configuration() -> Result<Arc<ProxyConfig>, String> {
    CONFIG
        .get_or_init(|| ProxyConfig::from_env().map(Arc::new))
        .as_ref()
        .map(Arc::clone)
        .map_err(Clone::clone)
}

pub(super) fn proxy_env() -> Result<Vec<(String, String)>, String> {
    Ok(configuration()?.environment())
}

/// A proxy URL with no credentials, query or path, safe to include in derived `Debug` output.
#[derive(Clone, Debug)]
struct Endpoint {
    uri: Uri,
}

impl Endpoint {
    fn parse(value: &str) -> Result<Option<Self>, String> {
        let value = value.trim();
        if value.is_empty() {
            return Ok(None);
        }
        let invalid = || {
            "proxy must be an http:// URL without credentials, a path, query or fragment".to_owned()
        };
        let uri: Uri = value.parse().map_err(|_error| invalid())?;
        let authority = uri.authority().ok_or_else(invalid)?;
        if uri.scheme_str() != Some("http")
            || authority.as_str().contains('@')
            || !matches!(uri.path(), "" | "/")
            || uri.query().is_some()
        {
            return Err(invalid());
        }
        let (_host, _port) = host_port(authority.as_str()).map_err(|()| invalid())?;
        Ok(Some(Self { uri }))
    }

    fn environment(&self) -> String {
        self.uri
            .authority()
            .map_or_else(String::new, |authority| format!("http://{authority}"))
    }
}

#[derive(Clone, Debug)]
struct ProxyConfig {
    http: Option<Endpoint>,
    https: Option<Endpoint>,
    no_proxy: NoProxy,
}

impl ProxyConfig {
    fn from_env() -> Result<Self, String> {
        let read = |lower: &str, upper: &str| -> Result<Option<String>, String> {
            // An explicitly empty lowercase value disables the uppercase setting too.
            std::env::var_os(lower)
                .or_else(|| std::env::var_os(upper))
                .map(|value| {
                    value
                        .into_string()
                        .map_err(|_value| format!("{lower} must be valid Unicode"))
                })
                .transpose()
        };
        Self::parse(
            read("http_proxy", "HTTP_PROXY")?.as_deref(),
            read("https_proxy", "HTTPS_PROXY")?.as_deref(),
            read("no_proxy", "NO_PROXY")?.as_deref(),
        )
    }

    fn parse(http: Option<&str>, https: Option<&str>, bypass: Option<&str>) -> Result<Self, String> {
        let normalized = normalize_bypass(bypass.unwrap_or_default());
        let mut no_proxy = NoProxy::parse(&normalized)?;
        // The chart uses the same normalization for controller env and admission. All
        // consumers receive exactly the list that the controller itself matches.
        no_proxy.environment = normalized;
        Ok(Self {
            http: Endpoint::parse(http.unwrap_or_default())?,
            https: Endpoint::parse(https.unwrap_or_default())?,
            no_proxy,
        })
    }

    fn endpoint(&self, uri: &Uri) -> Option<&Endpoint> {
        if self.no_proxy.matches(uri) {
            return None;
        }
        match uri.scheme_str() {
            Some("http") => self.http.as_ref(),
            Some("https") => self.https.as_ref(),
            _ => None,
        }
    }

    fn environment(&self) -> Vec<(String, String)> {
        if self.http.is_none() && self.https.is_none() {
            return Vec::new();
        }
        let http = self.http.as_ref().map_or_else(String::new, Endpoint::environment);
        let https = self.https.as_ref().map_or_else(String::new, Endpoint::environment);
        [
            ("http_proxy", http.clone()),
            ("HTTP_PROXY", http),
            ("https_proxy", https.clone()),
            ("HTTPS_PROXY", https),
            ("no_proxy", self.no_proxy.environment.clone()),
            ("NO_PROXY", self.no_proxy.environment.clone()),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
    }
}

fn normalize_bypass(provided: &str) -> String {
    let mut values = Vec::new();
    let mut changed = false;
    for value in provided.split(',').map(str::trim).filter(|v| !v.is_empty()) {
        if values.contains(&value) {
            changed = true;
        } else {
            values.push(value);
        }
    }
    for required in INTERNAL_HOSTS.split(',') {
        if !values.contains(&required) {
            values.push(required);
            changed = true;
        }
    }
    if changed { values.join(",") } else { provided.to_owned() }
}

#[derive(Clone, Debug)]
struct NoProxy {
    rules: Vec<Bypass>,
    environment: String,
}

#[derive(Clone, Debug)]
enum Bypass {
    All,
    Host(String, Option<u16>),
    Ip(IpAddr, Option<u16>),
    Network(IpAddr, u8),
}

impl NoProxy {
    fn parse(value: &str) -> Result<Self, String> {
        let values: Vec<_> = value.split(',').map(str::trim).filter(|v| !v.is_empty()).collect();
        let rules = values
            .iter()
            .map(|value| Bypass::parse(value))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            rules,
            environment: values.join(","),
        })
    }

    fn matches(&self, uri: &Uri) -> bool {
        let Some(host) = uri.host() else { return false };
        let host = host.trim_matches(['[', ']']).trim_end_matches('.').to_ascii_lowercase();
        let port = uri.port_u16().unwrap_or(if uri.scheme_str() == Some("https") { 443 } else { 80 });
        self.rules.iter().any(|rule| rule.matches(&host, port))
    }
}

impl Bypass {
    fn parse(value: &str) -> Result<Self, String> {
        let invalid = || "NO_PROXY contains an invalid host, IP, CIDR or port".to_owned();
        if value == "*" {
            return Ok(Self::All);
        }
        if let Some((ip, prefix)) = value.split_once('/') {
            let ip: IpAddr = ip.parse().map_err(|_error| invalid())?;
            let prefix: u8 = prefix.parse().map_err(|_error| invalid())?;
            if prefix > if ip.is_ipv4() { 32 } else { 128 } {
                return Err(invalid());
            }
            return Ok(Self::Network(ip, prefix));
        }
        let (host, port) = host_port(value).map_err(|()| invalid())?;
        let host = host.trim_matches(['[', ']']);
        if let Ok(ip) = host.parse::<IpAddr>() {
            return Ok(Self::Ip(ip, port));
        }
        let host = host.trim_start_matches('.').trim_end_matches('.').to_ascii_lowercase();
        if host.is_empty()
            || !host.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.'))
            || host.split('.').any(str::is_empty)
        {
            return Err(invalid());
        }
        Ok(Self::Host(host, port))
    }

    fn matches(&self, host: &str, port: u16) -> bool {
        match self {
            Self::All => true,
            Self::Host(name, expected_port) => {
                expected_port.is_none_or(|expected| expected == port)
                    && (host == name || host.strip_suffix(name).is_some_and(|s| s.ends_with('.')))
            }
            Self::Ip(ip, expected_port) => {
                expected_port.is_none_or(|expected| expected == port) && host.parse() == Ok(*ip)
            }
            Self::Network(network, prefix) => match (*network, host.parse::<IpAddr>()) {
                (IpAddr::V4(network), Ok(IpAddr::V4(ip))) => {
                    let mask = if *prefix == 0 { 0 } else { u32::MAX << (32 - u32::from(*prefix)) };
                    u32::from(network) & mask == u32::from(ip) & mask
                }
                (IpAddr::V6(network), Ok(IpAddr::V6(ip))) => {
                    let mask = if *prefix == 0 { 0 } else { u128::MAX << (128 - u32::from(*prefix)) };
                    u128::from(network) & mask == u128::from(ip) & mask
                }
                _ => false,
            },
        }
    }
}

fn host_port(value: &str) -> Result<(&str, Option<u16>), ()> {
    if value.parse::<IpAddr>().is_ok() {
        return Ok((value, None));
    }
    let (host, port) = if let Some(bracketed) = value.strip_prefix('[') {
        let (host, after) = bracketed.split_once(']').ok_or(())?;
        if host.parse::<std::net::Ipv6Addr>().is_err() {
            return Err(());
        }
        (host, if after.is_empty() { None } else { Some(after.strip_prefix(':').ok_or(())?) })
    } else if let Some((host, port)) = value.rsplit_once(':') {
        (host, Some(port))
    } else {
        (value, None)
    };
    if host.is_empty() {
        return Err(());
    }
    let port = port.map(|p| p.parse::<u16>().ok().filter(|p| *p != 0).ok_or(())).transpose()?;
    Ok((host, port))
}

/// A connector whose only proxy protocol is unauthenticated HTTP. No raw environment values
/// can appear in errors; authenticated URLs are refused at initialization.
#[derive(Clone, Debug)]
pub(crate) struct ProxyConnector {
    inner: HttpConnector,
    config: Arc<ProxyConfig>,
}

impl ProxyConnector {
    /// The connector using the process's validated startup configuration.
    pub(super) fn from_env() -> Result<Self, String> {
        Ok(Self::new(configuration()?))
    }

    fn new(config: Arc<ProxyConfig>) -> Self {
        let mut inner = HttpConnector::new();
        inner.enforce_http(false);
        inner.set_connect_timeout(Some(CONNECT_TIMEOUT));
        Self { inner, config }
    }
}

type ConnectFuture = Pin<Box<dyn Future<Output = Result<ProxyStream, io::Error>> + Send>>;

impl Service<Uri> for ProxyConnector {
    type Response = ProxyStream;
    type Error = io::Error;
    type Future = ConnectFuture;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(io::Error::other)
    }

    fn call(&mut self, target: Uri) -> Self::Future {
        let proxy = self.config.endpoint(&target).cloned();
        let mut inner = self.inner.clone();
        Box::pin(async move {
            let destination = proxy.as_ref().map_or_else(|| target.clone(), |p| p.uri.clone());
            let stream = inner.call(destination).await.map_err(io::Error::other)?;
            let forward = proxy.is_some() && target.scheme_str() == Some("http");
            if proxy.is_some() && !forward {
                tokio::time::timeout(CONNECT_TIMEOUT, tunnel(stream.inner(), &target))
                    .await
                    .map_err(|_error| io::Error::new(io::ErrorKind::TimedOut, "HTTP proxy CONNECT timed out"))??;
            }
            Ok(ProxyStream { io: stream, forward })
        })
    }
}

async fn tunnel(stream: &TcpStream, target: &Uri) -> io::Result<()> {
    let host = target.host().ok_or_else(|| io::Error::other("CONNECT target has no host"))?;
    let host = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host.to_owned() };
    let (_, port) = host_port(target.authority().ok_or_else(|| io::Error::other("CONNECT target has no authority"))?.as_str())
        .map_err(|()| io::Error::other("CONNECT target has an invalid port"))?;
    let authority = format!("{host}:{}", port.unwrap_or(443));
    let request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n");
    let mut remaining = request.as_bytes();
    while !remaining.is_empty() {
        stream.writable().await?;
        match stream.try_write(remaining) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "HTTP proxy closed CONNECT")),
            Ok(written) => remaining = remaining.get(written..).ok_or_else(|| io::Error::other("invalid CONNECT write"))?,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
    }
    // Read one byte at a time so no tunneled bytes are lost after the header boundary.
    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") {
        if headers.len() >= MAX_CONNECT_HEADERS {
            return Err(io::Error::other("HTTP proxy CONNECT headers are too large"));
        }
        stream.readable().await?;
        let mut byte = [0];
        match stream.try_read(&mut byte) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "HTTP proxy closed CONNECT")),
            Ok(_) => headers.extend_from_slice(&byte),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
    }
    connect_status(&headers)
}

fn connect_status(headers: &[u8]) -> io::Result<()> {
    let first = headers.split(|b| *b == b'\n').next().unwrap_or_default();
    let first = std::str::from_utf8(first).map_err(|_error| io::Error::other("invalid HTTP proxy CONNECT response"))?;
    let mut fields = first.split_ascii_whitespace();
    let version = fields.next();
    let status = fields.next().filter(|s| s.len() == 3 && s.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|s| s.parse::<u16>().ok());
    if !matches!(version, Some("HTTP/1.0" | "HTTP/1.1")) {
        return Err(io::Error::other("invalid HTTP proxy CONNECT response"));
    }
    match status {
        Some(200..=299) => Ok(()),
        Some(status) => Err(io::Error::other(format!("HTTP proxy rejected CONNECT: status {status}"))),
        None => Err(io::Error::other("invalid HTTP proxy CONNECT response")),
    }
}

/// Hyper I/O with absolute-form requests only for a plain HTTP forward proxy.
#[derive(Debug)]
pub(crate) struct ProxyStream {
    io: TokioIo<TcpStream>,
    forward: bool,
}

impl Connection for ProxyStream {
    fn connected(&self) -> Connected {
        self.io.connected().proxy(self.forward)
    }
}

impl Read for ProxyStream {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: ReadBufCursor<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl Write for ProxyStream {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_write_vectored(mut self: Pin<&mut Self>, cx: &mut Context<'_>, bufs: &[io::IoSlice<'_>]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write_vectored(cx, bufs)
    }
}

#[cfg(test)]
mod tests;
