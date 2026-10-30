//! Loopback-only proxy tests: no GitHub requests and no changes to process environment.

use std::error::Error;
use std::io::Write as _;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream as StdStream};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use bytes::Bytes;
use http::{Request, StatusCode};
use http_body_util::{BodyExt as _, Full};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use rcgen::{CertificateParams, KeyPair};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig, ServerConnection, StreamOwned};

use super::*;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;
type Server = (SocketAddr, JoinHandle<io::Result<String>>);

#[test]
fn configuration_rejects_unsupported_or_authenticated_proxies_without_disclosing_values() {
    for value in [
        "socks5://proxy.test:1080",
        "https://proxy.test:443",
        "http://username:private-password@proxy.test:3128",
        "http://proxy.test:3128/private-token",
        "http://proxy.test:3128/?token=private-token",
        "http://proxy.test:0",
        "http://proxy.test:99999",
    ] {
        let answer = ProxyConfig::parse(Some(value), None, None);
        assert!(answer.is_err(), "unsupported proxy accepted");
        assert!(!format!("{answer:?}").contains("private-"));
        assert!(!format!("{answer:?}").contains("username"));
    }
}

#[test]
fn no_proxy_domains_ports_and_networks() -> TestResult {
    let bypass =
        NoProxy::parse(".example.test,registry.test:5000,192.0.2.0/24,2001:db8::/32,[::1]:8443")?;
    for value in [
        "https://example.test/",
        "https://sub.EXAMPLE.TEST./",
        "http://registry.test:5000/v2/",
        "https://192.0.2.9/",
        "https://[2001:db8:42::1]/",
        "https://[::1]:8443/",
    ] {
        assert!(
            bypass.matches(&value.parse()?),
            "expected bypass for {value}"
        );
    }
    for value in [
        "https://notexample.test/",
        "https://example.test.attacker.test/",
        "http://registry.test:5001/",
        "https://192.0.3.9/",
        "https://[2001:db9::1]/",
        "https://[::1]:443/",
    ] {
        assert!(
            !bypass.matches(&value.parse()?),
            "unexpected bypass for {value}"
        );
    }
    for invalid in [
        "10.0.0.0/33",
        "::/129",
        "example.test:bad",
        "host:0",
        "host/path",
    ] {
        assert!(NoProxy::parse(invalid).is_err());
    }
    assert!(NoProxy::parse("*")?.matches(&"https://any.test/".parse()?));
    Ok(())
}

#[test]
fn generated_environment_keeps_internal_traffic_direct() -> TestResult {
    let config = ProxyConfig::parse(None, Some("http://100.66.31.168:3128"), None)?;
    for value in [
        "http://localhost/",
        "https://127.0.0.1:6443/",
        "https://[::1]/",
        "https://zot.zot.svc:5000/v2/",
        "https://buildkitd.builder-system.svc.cluster.local:1234/",
        "https://10.42.1.9/",
        "https://10.43.0.1/",
        "https://172.16.1.144/",
    ] {
        assert!(config.no_proxy.matches(&value.parse()?));
    }
    assert!(config.endpoint(&"https://github.com/".parse()?).is_some());
    let env = config.environment();
    assert!(
        env.iter()
            .any(|(key, value)| key == "http_proxy" && value.is_empty())
    );
    assert!(
        env.iter()
            .any(|(key, value)| key == "HTTPS_PROXY" && value == "http://100.66.31.168:3128")
    );
    let no_proxy = env
        .iter()
        .find(|(key, _)| key == "no_proxy")
        .map(|(_, value)| value);
    assert!(no_proxy.is_some_and(|value| value.contains(".svc")));
    assert_eq!(
        ProxyConfig::parse(None, None, None)?.environment(),
        Vec::<(String, String)>::new()
    );
    let supplied = format!("example.test,{INTERNAL_HOSTS}");
    let configured = ProxyConfig::parse(None, Some("http://100.66.31.168:3128"), Some(&supplied))?;
    assert!(
        configured
            .environment()
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case("no_proxy"))
            .all(|(_, value)| value == &supplied)
    );
    let incomplete = ProxyConfig::parse(
        None,
        Some("http://100.66.31.168:3128"),
        Some("example.test,example.test"),
    )?;
    let normalized = format!("example.test,{INTERNAL_HOSTS}");
    assert_eq!(incomplete.no_proxy.environment, normalized);
    let exported = NoProxy::parse(&incomplete.no_proxy.environment)?;
    for value in [
        "https://example.test/",
        "https://zot.zot.svc:5000/",
        "https://172.16.1.144/",
        "https://github.com/",
    ] {
        let uri = value.parse()?;
        assert_eq!(incomplete.no_proxy.matches(&uri), exported.matches(&uri));
    }
    Ok(())
}

#[test]
fn connect_status_rejects_errors_without_echoing_proxy_headers() {
    assert!(connect_status(b"HTTP/1.1 200 Connection established\r\n\r\n").is_ok());
    for response in [
        b"HTTP/1.1 407 Proxy Authentication Required\r\nX-Secret: private-token\r\n\r\n".as_slice(),
        b"HTTP/1.1 502 Bad Gateway\r\n\r\n".as_slice(),
        b"not-an-http-response\r\n\r\n".as_slice(),
    ] {
        let answer = connect_status(response);
        assert!(answer.is_err());
        assert!(!format!("{answer:?}").contains("private-token"));
    }
}

fn client(config: ProxyConfig, roots: RootCertStore) -> TestResult<super::super::HttpClient> {
    let tls = ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let connector = HttpsConnectorBuilder::new()
        .with_tls_config(tls)
        .https_or_http()
        .enable_http1()
        .wrap_connector(ProxyConnector::new(Arc::new(config)));
    Ok(Client::builder(TokioExecutor::new()).build(connector))
}

fn accept(listener: &TcpListener) -> io::Result<StdStream> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                stream.set_read_timeout(Some(Duration::from_secs(3)))?;
                stream.set_write_timeout(Some(Duration::from_secs(3)))?;
                return Ok(stream);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "test server was not contacted",
                    ));
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error),
        }
    }
}

fn headers(stream: &mut impl io::Read) -> io::Result<String> {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        if bytes.len() > MAX_CONNECT_HEADERS {
            return Err(io::Error::other("test request headers too large"));
        }
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        bytes.extend_from_slice(&byte);
    }
    String::from_utf8(bytes).map_err(io::Error::other)
}

fn plain_server() -> io::Result<Server> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    Ok((
        address,
        thread::spawn(move || {
            let mut stream = accept(&listener)?;
            let request = headers(&mut stream)?;
            stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            )?;
            Ok(request)
        }),
    ))
}

fn tls_server() -> TestResult<(Server, CertificateDer<'static>)> {
    let key = KeyPair::generate()?;
    let certificate =
        CertificateParams::new(vec!["origin.example.test".to_owned()])?.self_signed(&key)?;
    let der = certificate.der().clone();
    let config = ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(
        vec![der.clone()],
        PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
    )?;
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    let server = thread::spawn(move || {
        let socket = accept(&listener)?;
        let connection = ServerConnection::new(Arc::new(config)).map_err(io::Error::other)?;
        let mut stream = StreamOwned::new(connection, socket);
        let request = headers(&mut stream)?;
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")?;
        stream.conn.send_close_notify();
        stream.flush()?;
        Ok(request)
    });
    Ok(((address, server), der))
}

fn proxy_server(origin: SocketAddr) -> io::Result<Server> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    let server = thread::spawn(move || {
        let mut downstream = accept(&listener)?;
        let request = headers(&mut downstream)?;
        let mut upstream = StdStream::connect_timeout(&origin, Duration::from_secs(3))?;
        upstream.set_read_timeout(Some(Duration::from_secs(3)))?;
        upstream.set_write_timeout(Some(Duration::from_secs(3)))?;
        downstream.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")?;
        let mut to_client = downstream.try_clone()?;
        let mut from_origin = upstream.try_clone()?;
        let forward = thread::spawn(move || {
            let _copied = io::copy(&mut downstream, &mut upstream);
            let _closed = upstream.shutdown(Shutdown::Write);
        });
        let _copied = io::copy(&mut from_origin, &mut to_client);
        let _closed = to_client.shutdown(Shutdown::Write);
        forward
            .join()
            .map_err(|_panic| io::Error::other("proxy forwarding thread panicked"))?;
        Ok(request)
    });
    Ok((address, server))
}

async fn finish(server: JoinHandle<io::Result<String>>) -> TestResult<String> {
    let result = tokio::task::spawn_blocking(move || {
        server
            .join()
            .map_err(|_panic| io::Error::other("test server panicked"))
    })
    .await??;
    Ok(result?)
}

async fn fetch(client: &super::super::HttpClient, url: &str) -> TestResult {
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        client.request(Request::get(url).body(Full::new(Bytes::new()))?),
    )
    .await??;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.into_body().collect().await?.to_bytes(), "ok");
    Ok(())
}

#[tokio::test]
async fn https_connect_forwards_tls_and_preserves_target_certificate_validation() -> TestResult {
    let ((origin, origin_task), certificate) = tls_server()?;
    let (proxy, proxy_task) = proxy_server(origin)?;
    let mut roots = RootCertStore::empty();
    roots.add(certificate)?;
    let config = ProxyConfig::parse(None, Some(&format!("http://{proxy}")), None)?;
    let client = client(config, roots)?;
    fetch(&client, "https://origin.example.test/repository/info/refs").await?;
    drop(client);
    let connect = finish(proxy_task).await?;
    assert!(connect.starts_with("CONNECT origin.example.test:443 HTTP/1.1\r\n"));
    let request = finish(origin_task).await?;
    assert!(request.starts_with("GET /repository/info/refs HTTP/1.1\r\n"));
    Ok(())
}

async fn rejected_tls(host: &str, trust: bool) -> TestResult {
    let ((origin, origin_task), certificate) = tls_server()?;
    let (proxy, proxy_task) = proxy_server(origin)?;
    let mut roots = RootCertStore::empty();
    if trust {
        roots.add(certificate)?;
    }
    let config = ProxyConfig::parse(None, Some(&format!("http://{proxy}")), None)?;
    let client = client(config, roots)?;
    let request = Request::get(format!("https://{host}/")).body(Full::new(Bytes::new()))?;
    let answer = tokio::time::timeout(Duration::from_secs(5), client.request(request)).await?;
    assert!(answer.is_err(), "untrusted TLS identity was accepted");
    drop(client);
    assert!(finish(proxy_task).await?.starts_with("CONNECT "));
    // Rejection is expected to arrive at the origin as a TLS alert, before any HTTP request.
    let rejected = tokio::task::spawn_blocking(move || {
        origin_task
            .join()
            .map_err(|_panic| io::Error::other("TLS test server panicked"))
    })
    .await??;
    assert!(rejected.is_err());
    Ok(())
}

#[tokio::test]
async fn https_proxy_cannot_bypass_hostname_verification() -> TestResult {
    rejected_tls("wrong.example.test", true).await
}

#[tokio::test]
async fn https_proxy_cannot_bypass_ca_verification() -> TestResult {
    rejected_tls("origin.example.test", false).await
}

#[tokio::test]
async fn rejected_connect_never_falls_back_to_a_direct_connection() -> TestResult {
    let origin = TcpListener::bind(("127.0.0.1", 0))?;
    origin.set_nonblocking(true)?;
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    listener.set_nonblocking(true)?;
    let proxy = listener.local_addr()?;
    let proxy_task = thread::spawn(move || {
        let mut socket = accept(&listener)?;
        let request = headers(&mut socket)?;
        socket.write_all(
            b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n",
        )?;
        Ok(request)
    });
    let config = ProxyConfig {
        http: None,
        https: Endpoint::parse(&format!("http://{proxy}"))?,
        no_proxy: NoProxy::parse("")?,
    };
    let client = client(config, RootCertStore::empty())?;
    let request =
        Request::get(format!("https://{}/", origin.local_addr()?)).body(Full::new(Bytes::new()))?;
    let result = tokio::time::timeout(Duration::from_secs(5), client.request(request)).await?;
    assert!(result.is_err());
    drop(client);
    assert!(finish(proxy_task).await?.starts_with("CONNECT 127.0.0.1:"));
    assert!(
        origin
            .accept()
            .is_err_and(|error| error.kind() == io::ErrorKind::WouldBlock)
    );
    Ok(())
}

#[tokio::test]
async fn http_proxy_uses_absolute_form() -> TestResult {
    let (proxy, task) = plain_server()?;
    let config = ProxyConfig::parse(Some(&format!("http://{proxy}")), None, None)?;
    let client = client(config, RootCertStore::empty())?;
    fetch(&client, "http://public.example.test/path?q=1").await?;
    drop(client);
    assert!(
        finish(task)
            .await?
            .starts_with("GET http://public.example.test/path?q=1 HTTP/1.1\r\n")
    );
    Ok(())
}

#[tokio::test]
async fn absent_proxy_keeps_direct_connections() -> TestResult {
    let (address, task) = plain_server()?;
    let client = client(
        ProxyConfig::parse(None, None, None)?,
        RootCertStore::empty(),
    )?;
    fetch(&client, &format!("http://{address}/direct")).await?;
    drop(client);
    assert!(finish(task).await?.starts_with("GET /direct HTTP/1.1\r\n"));
    Ok(())
}

async fn bypass_network(value: &str, host: &str) -> TestResult {
    let (address, task) = plain_server()?;
    let config = ProxyConfig {
        http: Endpoint::parse("http://127.0.0.1:1")?,
        https: None,
        no_proxy: NoProxy::parse(value)?,
    };
    let client = client(config, RootCertStore::empty())?;
    fetch(&client, &format!("http://{host}:{}/direct", address.port())).await?;
    drop(client);
    assert!(finish(task).await?.starts_with("GET /direct HTTP/1.1\r\n"));
    Ok(())
}

#[tokio::test]
async fn no_proxy_domain_keeps_real_request_direct() -> TestResult {
    bypass_network("localhost", "localhost").await
}

#[tokio::test]
async fn no_proxy_cidr_keeps_real_request_direct() -> TestResult {
    bypass_network("127.0.0.0/8", "127.0.0.1").await
}
