//! In-memory rustls trust for dev tests.
//!
//! The CA is generated in this process and never written to disk or to the
//! operating-system trust store. Production builders keep using
//! `tls::native_roots`. A feature flag on those builders would still be a
//! production branch, so this type lives only in the test binary.
//!
//! Each integration-test binary uses a different subset of this module.
#![allow(dead_code)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};

/// Body the loopback server returns after a successful handshake.
pub const HARNESS_MARKER: &str = "udm-harness-ok";

/// A CA and a loopback server certificate that exist only in this process.
pub struct HarnessCa {
    ca_cert: CertificateDer<'static>,
    server_cert: CertificateDer<'static>,
    server_key_pkcs8: Vec<u8>,
}

impl std::fmt::Debug for HarnessCa {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HarnessCa")
            .field("ca_der_len", &self.ca_cert.as_ref().len())
            .field("server_der_len", &self.server_cert.as_ref().len())
            .finish_non_exhaustive()
    }
}

impl HarnessCa {
    /// Generate a new CA and a certificate for `127.0.0.1`.
    pub fn generate() -> Result<Self, String> {
        // The same process-wide `ring` provider the engine installs. The
        // harness config below carries its own provider either way.
        install_provider();

        let ca_key = KeyPair::generate().map_err(|e| format!("CA key: {e}"))?;
        let mut ca_params =
            CertificateParams::new(Vec::<String>::new()).map_err(|e| format!("CA params: {e}"))?;
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let mut ca_name = DistinguishedName::new();
        ca_name.push(DnType::CommonName, "UDM dev harness CA");
        ca_params.distinguished_name = ca_name;
        let ca_cert = ca_params
            .self_signed(&ca_key)
            .map_err(|e| format!("CA certificate: {e}"))?;

        let server_key = KeyPair::generate().map_err(|e| format!("server key: {e}"))?;
        let mut server_params = CertificateParams::new(Vec::<String>::new())
            .map_err(|e| format!("server params: {e}"))?;
        // An IP connection is checked against an iPAddress SAN. A DNS name of
        // "127.0.0.1" would not satisfy that check. HTTP/3 dials the name
        // `localhost`, so the same certificate also carries that DNS SAN.
        let localhost =
            rcgen::Ia5String::try_from("localhost").map_err(|e| format!("localhost SAN: {e}"))?;
        server_params.subject_alt_names = vec![
            SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            SanType::DnsName(localhost),
        ];
        server_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let mut server_name = DistinguishedName::new();
        server_name.push(DnType::CommonName, "127.0.0.1");
        server_params.distinguished_name = server_name;
        let server_cert = server_params
            .signed_by(&server_key, &ca_cert, &ca_key)
            .map_err(|e| format!("server certificate: {e}"))?;

        Ok(HarnessCa {
            ca_cert: ca_cert.der().clone(),
            server_cert: server_cert.der().clone(),
            server_key_pkcs8: server_key.serialize_der(),
        })
    }

    /// Client config that trusts this CA and nothing else.
    pub fn client_config(&self) -> Result<Arc<ClientConfig>, String> {
        let mut roots = RootCertStore::empty();
        roots
            .add(self.ca_cert.clone())
            .map_err(|e| format!("trust anchor: {e}"))?;
        let mut config =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .map_err(|e| format!("client protocol versions: {e}"))?
                .with_root_certificates(roots)
                .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Arc::new(config))
    }

    /// QUIC client config that trusts this CA and offers `h3`.
    ///
    /// TLS 1.3 only, which QUIC requires. The HTTP/1.1 config above is unchanged.
    pub fn quic_client_config(&self) -> Result<quinn::ClientConfig, String> {
        let mut config = self.tls13_client(vec![b"h3".to_vec()])?;
        config.enable_early_data = true;
        let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(config)
            .map_err(|e| format!("quic client config: {e}"))?;
        Ok(quinn::ClientConfig::new(Arc::new(crypto)))
    }

    /// QUIC server config presenting this process's loopback certificate.
    pub fn quic_server_config(&self) -> Result<quinn::ServerConfig, String> {
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.server_key_pkcs8.clone()));
        let mut config =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_protocol_versions(&[&rustls::version::TLS13])
                .map_err(|e| format!("server protocol versions: {e}"))?
                .with_no_client_auth()
                .with_single_cert(vec![self.server_cert.clone()], key)
                .map_err(|e| format!("server certificate: {e}"))?;
        config.alpn_protocols = vec![b"h3".to_vec()];
        config.max_early_data_size = u32::MAX;
        let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(config)
            .map_err(|e| format!("quic server config: {e}"))?;
        Ok(quinn::ServerConfig::with_crypto(Arc::new(crypto)))
    }

    /// Bind `127.0.0.1:0` and answer HTTP/3 on the certificate from this CA.
    pub async fn spawn_h3(&self) -> Result<QuicHarness, String> {
        let endpoint = quinn::Endpoint::server(
            self.quic_server_config()?,
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        )
        .map_err(|e| format!("quic bind: {e}"))?;
        let addr = endpoint
            .local_addr()
            .map_err(|e| format!("quic address: {e}"))?;
        let accepted = Arc::new(AtomicUsize::new(0));
        let failed = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let endpoint_task = endpoint.clone();
        let accepted_task = Arc::clone(&accepted);
        let failed_task = Arc::clone(&failed);
        let requests_task = Arc::clone(&requests);
        let join = tokio::spawn(async move {
            loop {
                let incoming = tokio::select! {
                    biased;
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                        continue;
                    }
                    incoming = endpoint_task.accept() => incoming,
                };
                let Some(incoming) = incoming else {
                    break;
                };
                let accepted_task = Arc::clone(&accepted_task);
                let failed_task = Arc::clone(&failed_task);
                let requests_task = Arc::clone(&requests_task);
                tokio::spawn(async move {
                    match incoming.await {
                        Ok(conn) => {
                            accepted_task.fetch_add(1, Ordering::SeqCst);
                            let _ = serve_h3(conn, &requests_task).await;
                        }
                        Err(_) => {
                            failed_task.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                });
            }
        });
        Ok(QuicHarness {
            addr,
            accepted,
            failed,
            requests,
            endpoint,
            shutdown: shutdown_tx,
            join,
        })
    }

    /// Bind `127.0.0.1:0` and serve `payload` over HTTP/3.
    ///
    /// When `hold_body` is set, a GET's headers go out immediately and the
    /// body waits for [`FileQuic::release_bodies`]. HEAD does not wait.
    pub async fn spawn_h3_file(
        &self,
        payload: Vec<u8>,
        hold_body: bool,
    ) -> Result<FileQuic, String> {
        let endpoint = quinn::Endpoint::server(
            self.quic_server_config()?,
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        )
        .map_err(|e| format!("quic bind: {e}"))?;
        let addr = endpoint
            .local_addr()
            .map_err(|e| format!("quic address: {e}"))?;
        let accepted = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));
        let goaway_applied = Arc::new(AtomicBool::new(false));
        let payload = Arc::new(payload);
        let (release, _release_rx) = tokio::sync::watch::channel(!hold_body);
        let (goaway, _goaway_rx) = tokio::sync::watch::channel(false);
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let endpoint_task = endpoint.clone();
        let accepted_task = Arc::clone(&accepted);
        let requests_task = Arc::clone(&requests);
        let applied_task = Arc::clone(&goaway_applied);
        let release_task = release.clone();
        let goaway_task = goaway.clone();
        let join = tokio::spawn(async move {
            loop {
                let incoming = tokio::select! {
                    biased;
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                        continue;
                    }
                    incoming = endpoint_task.accept() => incoming,
                };
                let Some(incoming) = incoming else {
                    break;
                };
                let accepted_task = Arc::clone(&accepted_task);
                let requests_task = Arc::clone(&requests_task);
                let applied_task = Arc::clone(&applied_task);
                let payload = Arc::clone(&payload);
                let release_rx = release_task.subscribe();
                let mut goaway_rx = goaway_task.subscribe();
                let mut shutdown_conn = shutdown_rx.clone();
                tokio::spawn(async move {
                    let conn = match incoming.await {
                        Ok(conn) => conn,
                        Err(_) => return,
                    };
                    accepted_task.fetch_add(1, Ordering::SeqCst);
                    let mut h3_conn = match h3::server::builder()
                        .build(h3_quinn::Connection::new(conn))
                        .await
                    {
                        Ok(conn) => conn,
                        Err(_) => return,
                    };
                    loop {
                        tokio::select! {
                            biased;
                            changed = shutdown_conn.changed() => {
                                if changed.is_err() || *shutdown_conn.borrow() {
                                    break;
                                }
                            }
                            changed = goaway_rx.changed() => {
                                if changed.is_ok() && *goaway_rx.borrow() {
                                    let _ = h3_conn.shutdown(0).await;
                                    applied_task.store(true, Ordering::SeqCst);
                                }
                            }
                            incoming = h3_conn.accept() => {
                                match incoming {
                                    Ok(Some(resolver)) => {
                                        requests_task.fetch_add(1, Ordering::SeqCst);
                                        let payload = Arc::clone(&payload);
                                        let release_rx = release_rx.clone();
                                        tokio::spawn(async move {
                                            let _ = answer_h3_file(resolver, &payload, release_rx).await;
                                        });
                                    }
                                    Ok(None) | Err(_) => break,
                                }
                            }
                        }
                    }
                });
            }
        });
        Ok(FileQuic {
            addr,
            accepted,
            requests,
            release,
            goaway,
            goaway_applied,
            endpoint,
            shutdown: shutdown_tx,
            join,
        })
    }

    fn tls13_client(&self, alpn: Vec<Vec<u8>>) -> Result<ClientConfig, String> {
        let mut roots = RootCertStore::empty();
        roots
            .add(self.ca_cert.clone())
            .map_err(|e| format!("trust anchor: {e}"))?;
        let mut config =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_protocol_versions(&[&rustls::version::TLS13])
                .map_err(|e| format!("client protocol versions: {e}"))?
                .with_root_certificates(roots)
                .with_no_client_auth();
        config.alpn_protocols = alpn;
        Ok(config)
    }

    /// DER of this process's CA, for a test client that is not `Http12Client`.
    pub fn ca_der(&self) -> &[u8] {
        self.ca_cert.as_ref()
    }

    /// Client config that trusts this CA only, with the caller's ALPN list.
    ///
    /// [`Self::client_config`] stays `http/1.1`. The I4 spike passes `h2`.
    pub fn client_config_alpn(&self, alpn: &[&[u8]]) -> Result<ClientConfig, String> {
        let mut roots = RootCertStore::empty();
        roots
            .add(self.ca_cert.clone())
            .map_err(|e| format!("trust anchor: {e}"))?;
        let mut config =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .map_err(|e| format!("client protocol versions: {e}"))?
                .with_root_certificates(roots)
                .with_no_client_auth();
        config.alpn_protocols = alpn.iter().map(|proto| proto.to_vec()).collect();
        Ok(config)
    }

    /// Server config presenting this process's loopback certificate.
    pub fn server_config(&self) -> Result<Arc<ServerConfig>, String> {
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.server_key_pkcs8.clone()));
        let mut config =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .map_err(|e| format!("server protocol versions: {e}"))?
                .with_no_client_auth()
                .with_single_cert(vec![self.server_cert.clone()], key)
                .map_err(|e| format!("server certificate: {e}"))?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Arc::new(config))
    }

    /// Same certificate as [`Self::server_config`], with the caller's ALPN list.
    pub fn server_config_alpn(&self, alpn: &[&[u8]]) -> Result<Arc<ServerConfig>, String> {
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.server_key_pkcs8.clone()));
        let mut config =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .map_err(|e| format!("server protocol versions: {e}"))?
                .with_no_client_auth()
                .with_single_cert(vec![self.server_cert.clone()], key)
                .map_err(|e| format!("server certificate: {e}"))?;
        config.alpn_protocols = alpn.iter().map(|proto| proto.to_vec()).collect();
        Ok(Arc::new(config))
    }

    /// Bind `127.0.0.1:0` and answer one HTTP/1.1 request per accepted handshake.
    pub async fn spawn_http(&self) -> Result<HarnessServer, String> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .await
            .map_err(|e| format!("bind: {e}"))?;
        let addr = listener
            .local_addr()
            .map_err(|e| format!("local address: {e}"))?;
        let acceptor = TlsAcceptor::from(self.server_config()?);
        let (shutdown, mut flag) = tokio::sync::watch::channel(false);
        let join = tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    biased;
                    changed = flag.changed() => {
                        if changed.is_err() || *flag.borrow() {
                            break;
                        }
                        continue;
                    }
                    accepted = listener.accept() => accepted,
                };
                let Ok((tcp, _)) = accepted else {
                    break;
                };
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    // A client that rejects the certificate ends here. That is
                    // the production path, so it must not stop the listener.
                    if let Ok(mut tls) = acceptor.accept(tcp).await {
                        let _ = serve_one(&mut tls).await;
                    }
                });
            }
        });
        Ok(HarnessServer {
            addr,
            shutdown,
            join,
        })
    }

    /// Fetch `path` with a client that trusts only this CA.
    pub async fn get(&self, addr: SocketAddr, path: &str) -> Result<String, String> {
        let tcp = TcpStream::connect(addr)
            .await
            .map_err(|e| format!("connect {addr}: {e}"))?;
        let connector = TlsConnector::from(self.client_config()?);
        let name = ServerName::IpAddress(rustls::pki_types::IpAddr::from(addr.ip()));
        let mut tls = connector
            .connect(name, tcp)
            .await
            .map_err(|e| format!("harness handshake: {e}"))?;
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            addr.ip()
        );
        tls.write_all(request.as_bytes())
            .await
            .map_err(|e| format!("request: {e}"))?;
        let mut buf = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), tls.read_to_end(&mut buf))
            .await
            .map_err(|_| "harness read timed out".to_string())?
            .map_err(|e| format!("response: {e}"))?;
        String::from_utf8(buf).map_err(|e| format!("response is not utf-8: {e}"))
    }
}

/// Loopback listener. Dropping it does not uninstall any trust anchor.
pub struct HarnessServer {
    pub addr: SocketAddr,
    shutdown: tokio::sync::watch::Sender<bool>,
    join: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for HarnessServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HarnessServer")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

impl HarnessServer {
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(2), self.join).await;
    }
}

/// Loopback QUIC listener. The certificate is the harness CA's; nothing is
/// written to the operating-system trust store.
pub struct QuicHarness {
    pub addr: SocketAddr,
    accepted: Arc<AtomicUsize>,
    failed: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
    endpoint: quinn::Endpoint,
    shutdown: tokio::sync::watch::Sender<bool>,
    join: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for QuicHarness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuicHarness")
            .field("addr", &self.addr)
            .field("accepted", &self.accepted())
            .field("failed_handshakes", &self.failed_handshakes())
            .field("requests", &self.requests())
            .finish_non_exhaustive()
    }
}

impl QuicHarness {
    /// QUIC handshakes that completed.
    pub fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }

    /// QUIC handshakes the server rejected.
    pub fn failed_handshakes(&self) -> usize {
        self.failed.load(Ordering::SeqCst)
    }

    /// HTTP/3 requests accepted on those connections.
    pub fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    pub async fn shutdown(self) {
        let _ = self.shutdown.send(true);
        self.endpoint
            .close(quinn::VarInt::from_u32(0), b"harness shutdown");
        let _ = tokio::time::timeout(Duration::from_secs(2), self.join).await;
    }
}

/// HTTP/3 file server for the I3 use-axis proofs.
///
/// `hold_body` sends response headers and then waits. HEAD never waits, so a
/// probe can finish while segment bodies stay on the network. `goaway` asks
/// the accepted connection to send GOAWAY and accept no further request.
pub struct FileQuic {
    pub addr: SocketAddr,
    accepted: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
    release: tokio::sync::watch::Sender<bool>,
    goaway: tokio::sync::watch::Sender<bool>,
    goaway_applied: Arc<AtomicBool>,
    endpoint: quinn::Endpoint,
    shutdown: tokio::sync::watch::Sender<bool>,
    join: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for FileQuic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileQuic")
            .field("addr", &self.addr)
            .field("accepted", &self.accepted())
            .field("requests", &self.requests())
            .finish_non_exhaustive()
    }
}

impl FileQuic {
    pub fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }

    pub fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    /// Let held GET bodies finish.
    pub fn release_bodies(&self) {
        let _ = self.release.send(true);
    }

    /// Send GOAWAY on the accepted connection and wait until that call returns.
    pub async fn goaway(&self) -> Result<(), String> {
        let _ = self.goaway.send(true);
        for _ in 0..100 {
            if self.goaway_applied.load(Ordering::SeqCst) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Err("server did not apply GOAWAY".to_string())
    }

    pub async fn shutdown(self) {
        let _ = self.shutdown.send(true);
        self.endpoint
            .close(quinn::VarInt::from_u32(0), b"harness shutdown");
        let _ = tokio::time::timeout(Duration::from_secs(2), self.join).await;
    }
}

async fn serve_h3(conn: quinn::Connection, requests: &AtomicUsize) -> Result<(), String> {
    let mut h3_conn = h3::server::builder()
        .build(h3_quinn::Connection::new(conn))
        .await
        .map_err(|e| format!("h3 server: {e}"))?;
    loop {
        match h3_conn.accept().await {
            Ok(Some(resolver)) => {
                requests.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let _ = answer_h3(resolver).await;
                });
            }
            Ok(None) | Err(_) => break,
        }
    }
    Ok(())
}

async fn answer_h3(
    resolver: h3::server::RequestResolver<h3_quinn::Connection, bytes::Bytes>,
) -> Result<(), String> {
    let (_request, mut stream) = resolver
        .resolve_request()
        .await
        .map_err(|e| format!("request: {e}"))?;
    let response = http::Response::builder()
        .status(http::StatusCode::OK)
        .header(http::header::CONTENT_LENGTH, HARNESS_MARKER.len())
        .body(())
        .map_err(|e| format!("response: {e}"))?;
    stream
        .send_response(response)
        .await
        .map_err(|e| format!("send response: {e}"))?;
    stream
        .send_data(bytes::Bytes::from_static(HARNESS_MARKER.as_bytes()))
        .await
        .map_err(|e| format!("send body: {e}"))?;
    stream.finish().await.map_err(|e| format!("finish: {e}"))?;
    Ok(())
}

async fn answer_h3_file(
    resolver: h3::server::RequestResolver<h3_quinn::Connection, bytes::Bytes>,
    payload: &[u8],
    release: tokio::sync::watch::Receiver<bool>,
) -> Result<(), String> {
    let (request, mut stream) = resolver
        .resolve_request()
        .await
        .map_err(|e| format!("request: {e}"))?;
    let part = ranged_body(&request, payload);
    let (status, body, content_range) = match &part {
        Some((start, end, body)) => (
            http::StatusCode::PARTIAL_CONTENT,
            *body,
            Some(format!("bytes {start}-{end}/{}", payload.len())),
        ),
        None => (http::StatusCode::OK, payload, None),
    };
    let mut builder = http::Response::builder()
        .status(status)
        .header(http::header::CONTENT_LENGTH, body.len())
        .header(http::header::ACCEPT_RANGES, "bytes")
        .header(http::header::ETAG, "\"fixed\"");
    if let Some(content_range) = content_range {
        builder = builder.header(http::header::CONTENT_RANGE, content_range);
    }
    let response = builder.body(()).map_err(|e| format!("response: {e}"))?;
    stream
        .send_response(response)
        .await
        .map_err(|e| format!("send response: {e}"))?;
    if request.method() == http::Method::HEAD {
        stream
            .finish()
            .await
            .map_err(|e| format!("finish head: {e}"))?;
        return Ok(());
    }
    wait_for_body_release(release).await;
    let mut offset = 0;
    while offset < body.len() {
        let end = (offset + 16 * 1024).min(body.len());
        stream
            .send_data(bytes::Bytes::copy_from_slice(&body[offset..end]))
            .await
            .map_err(|e| format!("send body: {e}"))?;
        offset = end;
    }
    stream.finish().await.map_err(|e| format!("finish: {e}"))?;
    Ok(())
}

fn ranged_body<'a>(request: &http::Request<()>, payload: &'a [u8]) -> Option<(u64, u64, &'a [u8])> {
    let header = request.headers().get(http::header::RANGE)?.to_str().ok()?;
    let (start, end) = parse_byte_range(header, payload.len())?;
    Some((start, end, &payload[start as usize..=end as usize]))
}

fn parse_byte_range(header: &str, len: usize) -> Option<(u64, u64)> {
    let rest = header.trim().strip_prefix("bytes=")?;
    let (start, end) = rest.split_once('-')?;
    let start: u64 = start.trim().parse().ok()?;
    let end = if end.trim().is_empty() {
        len.saturating_sub(1) as u64
    } else {
        end.trim().parse().ok()?
    };
    if start > end || end as usize >= len {
        return None;
    }
    Some((start, end))
}

async fn wait_for_body_release(mut release: tokio::sync::watch::Receiver<bool>) {
    while !*release.borrow() {
        if release.changed().await.is_err() {
            return;
        }
    }
}

fn install_provider() {
    // `tls::install_default_provider` is the engine's installer. Calling it
    // does not add this CA to any production config.
    let _ = udm_core::net::tls::install_default_provider();
}

async fn serve_one<IO>(tls: &mut tokio_rustls::server::TlsStream<IO>) -> Result<(), String>
where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut buf = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !buf.ends_with(b"\r\n\r\n") {
        if tokio::time::Instant::now() >= deadline {
            return Err("request headers timed out".to_string());
        }
        let n = tokio::time::timeout(Duration::from_secs(5), tls.read(&mut byte))
            .await
            .map_err(|_| "request read timed out".to_string())?
            .map_err(|e| format!("read request: {e}"))?;
        if n == 0 {
            return Err("client closed before headers".to_string());
        }
        buf.push(byte[0]);
        if buf.len() > 8192 {
            return Err("request headers too large".to_string());
        }
    }
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{HARNESS_MARKER}",
        HARNESS_MARKER.len()
    );
    tls.write_all(response.as_bytes())
        .await
        .map_err(|e| format!("write response: {e}"))?;
    tls.shutdown()
        .await
        .map_err(|e| format!("close response: {e}"))?;
    Ok(())
}
