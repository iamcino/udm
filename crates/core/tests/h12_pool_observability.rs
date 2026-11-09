//! I4 spike. Not a gauge.
//!
//! Asks the pinned client (reqwest 0.13.5 over hyper-util 0.1.21) whether a
//! pooled H1 or H2 connection reports checkout, negotiated version, return to
//! idle, and close. The harness CA stays in this test binary. `Http12Client::new`
//! is not modified and is not given that CA.

#[path = "common/mod.rs"]
mod common;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::dev_tls::{HarnessCa, HARNESS_MARKER};
use http_body_util::BodyExt;
use hyper_util::client::legacy::connect::{capture_connection, HttpInfo};
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioTimer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const IDLE_WAIT: Duration = Duration::from_millis(200);
const PEER_WAIT: Duration = Duration::from_millis(800);

#[tokio::test]
async fn h1_second_request_reuses_without_a_second_dial() -> Result<(), String> {
    let server = CleartextH1::bind(0).await?;
    let dials = Arc::new(AtomicUsize::new(0));
    let client = h1_client(dials.clone(), None)?;

    let first = fetch(&client, server.addr).await?;
    let second = fetch(&client, server.addr).await?;

    if first.version != http::Version::HTTP_11 || second.version != http::Version::HTTP_11 {
        return Err(format!(
            "cleartext responses were {:?} and {:?}, expected HTTP/1.1",
            first.version, second.version
        ));
    }
    if server.accepted() != 1 {
        return Err(format!(
            "server accepted {} TCP connections for two sequential requests",
            server.accepted()
        ));
    }
    if dials.load(Ordering::SeqCst) != 1 {
        return Err(format!(
            "connector layer ran {} times; the second request was a checkout the layer did not see, or the layer ran on the checkout",
            dials.load(Ordering::SeqCst)
        ));
    }
    match (first.local, second.local) {
        (Some(left), Some(right)) if left == right => {}
        (Some(left), Some(right)) => {
            return Err(format!(
                "sequential responses did not share a local socket: {left} then {right}"
            ));
        }
        _ => {
            return Err(
                "HttpInfo was absent, so the response does not carry the socket either".to_string(),
            );
        }
    }
    if server.closed() != 0 {
        return Err(format!(
            "server saw {} closes while both responses were still inside the idle timeout",
            server.closed()
        ));
    }
    Ok(())
}

#[tokio::test]
async fn h1_idle_timeout_closes_on_the_server_without_a_client_event() -> Result<(), String> {
    let server = CleartextH1::bind(0).await?;
    let dials = Arc::new(AtomicUsize::new(0));
    let client = h1_client(dials.clone(), Some(IDLE_WAIT))?;

    let first = fetch(&client, server.addr).await?;
    if dials.load(Ordering::SeqCst) != 1 || server.accepted() != 1 {
        return Err(format!(
            "setup dials={} accepted={}",
            dials.load(Ordering::SeqCst),
            server.accepted()
        ));
    }
    wait_until(|| server.closed() >= 1).await?;
    if dials.load(Ordering::SeqCst) != 1 {
        return Err(format!(
            "dial counter moved to {} when the server observed the close",
            dials.load(Ordering::SeqCst)
        ));
    }
    let second = fetch(&client, server.addr).await?;
    if server.accepted() != 2 || dials.load(Ordering::SeqCst) != 2 {
        return Err(format!(
            "after the idle close, accepted={} dials={}; a new dial is not a close event",
            server.accepted(),
            dials.load(Ordering::SeqCst)
        ));
    }
    match (first.local, second.local) {
        (Some(left), Some(right)) if left != right => Ok(()),
        other => Err(format!(
            "expected a new local socket after the idle close, got {other:?}"
        )),
    }
}

#[tokio::test]
async fn h1_parallel_first_requests_are_not_single_flight() -> Result<(), String> {
    let server = CleartextH1::bind(8).await?;
    let dials = Arc::new(AtomicUsize::new(0));
    let client = h1_client(dials.clone(), None)?;
    let samples = join_fetches(&client, server.addr, 8).await?;
    if samples.len() != 8 {
        return Err(format!("joined {} H1 responses", samples.len()));
    }
    if samples
        .iter()
        .any(|sample| sample.version != http::Version::HTTP_11)
    {
        return Err(format!(
            "a parallel H1 response was not HTTP/1.1: {samples:?}"
        ));
    }
    if server.accepted() != 8 || dials.load(Ordering::SeqCst) != 8 {
        return Err(format!(
            "eight held H1 requests: server accepted {}, connector dials {}",
            server.accepted(),
            dials.load(Ordering::SeqCst)
        ));
    }
    let ports: Vec<_> = samples.iter().filter_map(|sample| sample.local).collect();
    if ports.len() != 8 || unique_sockets(&ports) != 8 {
        return Err(format!("expected 8 distinct local sockets, got {ports:?}"));
    }
    Ok(())
}

#[tokio::test]
async fn h2_eight_streams_share_one_tls_session_and_report_version_on_the_response(
) -> Result<(), String> {
    let ca = HarnessCa::generate()?;
    let server = H2Listen::bind(&ca, 0).await?;
    let dials = Arc::new(AtomicUsize::new(0));
    let client = h2_client(&ca, dials.clone(), None)?;

    let _warm = https_fetch(&client, server.addr).await?;
    if server.tls_ok() != 1 || dials.load(Ordering::SeqCst) != 1 {
        return Err(format!(
            "warm-up tls={} dials={}",
            server.tls_ok(),
            dials.load(Ordering::SeqCst)
        ));
    }

    let samples = join_https(&client, server.addr, 8).await?;
    if samples.len() != 8 {
        return Err(format!("joined {} H2 responses", samples.len()));
    }
    if samples
        .iter()
        .any(|sample| sample.version != http::Version::HTTP_2)
    {
        return Err(format!("an H2 response did not report HTTP_2: {samples:?}"));
    }
    if server.tls_ok() != 1 || server.h2_ok() != 1 || dials.load(Ordering::SeqCst) != 1 {
        return Err(format!(
            "eight streams moved the connection count: tls={} h2={} dials={}",
            server.tls_ok(),
            server.h2_ok(),
            dials.load(Ordering::SeqCst)
        ));
    }
    let ports: Vec<_> = samples.iter().filter_map(|sample| sample.local).collect();
    if ports.len() != 8 || unique_sockets(&ports) != 1 {
        return Err(format!(
            "eight H2 responses did not share one local socket: {ports:?}"
        ));
    }
    if server.h2_live() != 1 {
        return Err(format!(
            "expected the shared H2 session to stay up, live={}",
            server.h2_live()
        ));
    }
    Ok(())
}

#[tokio::test]
async fn h2_parallel_first_dials_are_not_collapsed_before_connect() -> Result<(), String> {
    let ca = HarnessCa::generate()?;
    // Hold every TLS handshake until eight TCP accepts arrive, or 800ms passes.
    // A pool that single-flights before dialing never opens the other seven.
    let server = H2Listen::bind(&ca, 8).await?;
    let dials = Arc::new(AtomicUsize::new(0));
    let client = h2_client(&ca, dials.clone(), None)?;
    let samples = join_https(&client, server.addr, 8).await?;
    if samples.len() != 8
        || samples
            .iter()
            .any(|sample| sample.version != http::Version::HTTP_2)
    {
        return Err(format!(
            "parallel H2 burst did not all complete as HTTP_2: {samples:?}"
        ));
    }
    // The pinned ALPN path takes the H2 connecting lock only after the TLS
    // handshake (`Connecting::alpn_h2`). All eight dials are real.
    if server.tcp_accepts() != 8 || server.tls_ok() != 8 || dials.load(Ordering::SeqCst) != 8 {
        return Err(format!(
            "parallel first H2 dials: tcp={} tls={} h2_handshakes={} dials={}",
            server.tcp_accepts(),
            server.tls_ok(),
            server.h2_ok(),
            dials.load(Ordering::SeqCst)
        ));
    }
    wait_until(|| server.h2_live() == 1).await?;
    let kept = server.h2_ok();
    let after = https_fetch(&client, server.addr).await?;
    if after.version != http::Version::HTTP_2 {
        return Err(format!("follow-up version {:?}", after.version));
    }
    if server.tls_ok() != 8 || dials.load(Ordering::SeqCst) != 8 {
        return Err(format!(
            "follow-up dialed again: tls={} dials={}",
            server.tls_ok(),
            dials.load(Ordering::SeqCst)
        ));
    }
    // Losers are dropped before `h2_builder.handshake`, so the server should
    // finish one HTTP/2 preface. `tls_ok` stays 8: those handshakes happened.
    if server.h2_live() != 1 || kept != 1 {
        return Err(format!(
            "pool did not settle on one HTTP/2 preface: live={} h2_ok={kept}",
            server.h2_live()
        ));
    }
    Ok(())
}

#[tokio::test]
async fn hyper_util_capture_is_a_snapshot_without_checkout_or_close() -> Result<(), String> {
    let server = CleartextH1::bind(0).await?;
    let client: Client<_, http_body_util::Empty<bytes::Bytes>> =
        Client::builder(TokioExecutor::new())
            .pool_idle_timeout(IDLE_WAIT)
            .pool_timer(TokioTimer::new())
            .timer(TokioTimer::new())
            .build_http();

    let (first_capture, first_port, first_version) =
        hyper_fetch(&client, &format!("http://{}/", server.addr)).await?;
    let (second_capture, second_port, second_version) =
        hyper_fetch(&client, &format!("http://{}/", server.addr)).await?;

    if first_version != http::Version::HTTP_11 || second_version != http::Version::HTTP_11 {
        return Err(format!(
            "hyper-util versions {first_version:?} {second_version:?}"
        ));
    }
    if server.accepted() != 1 {
        return Err(format!(
            "hyper-util H1 reuse accepted {}",
            server.accepted()
        ));
    }
    let first_h2 = negotiated_h2(&first_capture).ok_or("first capture was empty")?;
    let second_h2 = negotiated_h2(&second_capture).ok_or("second capture was empty")?;
    if first_h2 || second_h2 {
        return Err(format!(
            "cleartext captures reported negotiated h2: {first_h2} {second_h2}"
        ));
    }
    if first_port != second_port {
        return Err(format!(
            "reused H1 sockets differed: {first_port:?} {second_port:?}"
        ));
    }
    // Both captures answer the same two questions. Checkout is not one of them.
    if proxied(&first_capture) != Some(false) || proxied(&second_capture) != Some(false) {
        return Err("cleartext capture reported a proxy".to_string());
    }

    wait_until(|| server.closed() >= 1).await?;
    let still = negotiated_h2(&first_capture).ok_or("capture cleared itself on close")?;
    if still {
        return Err("stale capture flipped to h2".to_string());
    }
    let (_third_capture, third_port, _) =
        hyper_fetch(&client, &format!("http://{}/", server.addr)).await?;
    if server.accepted() != 2 {
        return Err(format!(
            "idle expiry did not force a new TCP connection: accepted {}",
            server.accepted()
        ));
    }
    if third_port == first_port {
        return Err(format!(
            "new connection kept the old local socket {third_port:?}"
        ));
    }
    // The first watch value is still the original snapshot after the server
    // has already closed that socket.
    if negotiated_h2(&first_capture).is_none() {
        return Err("first capture disappeared when the socket closed".to_string());
    }

    let ca = HarnessCa::generate()?;
    let h2_server = H2Listen::bind(&ca, 0).await?;
    // hyper-rustls 0.27.10 panics when ALPN is already set. `enable_http2` writes `h2`.
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(ca.client_config_alpn(&[])?)
        .https_only()
        .enable_http2()
        .build();
    let h2_client: Client<_, http_body_util::Empty<bytes::Bytes>> =
        Client::builder(TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_timer(TokioTimer::new())
            .timer(TokioTimer::new())
            .build(https);
    let (h2_capture, _, h2_version) = hyper_fetch(
        &h2_client,
        &format!("https://127.0.0.1:{}/", h2_server.addr.port()),
    )
    .await?;
    if h2_version != http::Version::HTTP_2 {
        return Err(format!("hyper-util H2 response version {h2_version:?}"));
    }
    let negotiated = negotiated_h2(&h2_capture).ok_or("H2 capture was empty")?;
    if !negotiated {
        return Err("ALPN h2 capture did not report negotiated_h2".to_string());
    }
    if h2_server.tls_ok() != 1 {
        return Err(format!("H2 capture dialed {} times", h2_server.tls_ok()));
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct Sample {
    version: http::Version,
    local: Option<SocketAddr>,
}

async fn fetch(client: &reqwest::Client, addr: SocketAddr) -> Result<Sample, String> {
    let response = client
        .get(format!("http://{addr}/"))
        .send()
        .await
        .map_err(|e| format!("h1 send: {e}"))?;
    sample_reqwest(response).await
}

async fn https_fetch(client: &reqwest::Client, addr: SocketAddr) -> Result<Sample, String> {
    let response = client
        .get(format!("https://127.0.0.1:{}/", addr.port()))
        .send()
        .await
        .map_err(|e| format!("h2 send: {e}"))?;
    sample_reqwest(response).await
}

async fn sample_reqwest(response: reqwest::Response) -> Result<Sample, String> {
    let version = response.version();
    let local = response
        .extensions()
        .get::<HttpInfo>()
        .map(HttpInfo::local_addr);
    let body = response.text().await.map_err(|e| format!("body: {e}"))?;
    if !body.contains(HARNESS_MARKER) {
        return Err(format!("body {body:?}"));
    }
    Ok(Sample { version, local })
}

async fn join_fetches(
    client: &reqwest::Client,
    addr: SocketAddr,
    n: usize,
) -> Result<Vec<Sample>, String> {
    let barrier = Arc::new(tokio::sync::Barrier::new(n));
    let mut tasks = Vec::with_capacity(n);
    for _ in 0..n {
        let client = client.clone();
        let barrier = Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            fetch(&client, addr).await
        }));
    }
    join_samples(tasks).await
}

async fn join_https(
    client: &reqwest::Client,
    addr: SocketAddr,
    n: usize,
) -> Result<Vec<Sample>, String> {
    let barrier = Arc::new(tokio::sync::Barrier::new(n));
    let mut tasks = Vec::with_capacity(n);
    for _ in 0..n {
        let client = client.clone();
        let barrier = Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            https_fetch(&client, addr).await
        }));
    }
    join_samples(tasks).await
}

async fn join_samples(
    tasks: Vec<tokio::task::JoinHandle<Result<Sample, String>>>,
) -> Result<Vec<Sample>, String> {
    let mut samples = Vec::with_capacity(tasks.len());
    for task in tasks {
        samples.push(
            task.await
                .map_err(|e| format!("join: {e}"))?
                .map_err(|e| e)?,
        );
    }
    Ok(samples)
}

fn h1_client(dials: Arc<AtomicUsize>, idle: Option<Duration>) -> Result<reqwest::Client, String> {
    let _ = usagi::net::tls::install_default_provider();
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .pool_max_idle_per_host(8)
        .connector_layer(DialLayer { dials })
        .use_rustls_tls();
    if let Some(idle) = idle {
        builder = builder.pool_idle_timeout(idle);
    }
    builder.build().map_err(|e| format!("h1 client: {e}"))
}

fn h2_client(
    ca: &HarnessCa,
    dials: Arc<AtomicUsize>,
    idle: Option<Duration>,
) -> Result<reqwest::Client, String> {
    let _ = usagi::net::tls::install_default_provider();
    let cert = reqwest::Certificate::from_der(ca.ca_der()).map_err(|e| format!("ca der: {e}"))?;
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .pool_max_idle_per_host(8)
        .add_root_certificate(cert)
        .connector_layer(DialLayer { dials })
        .use_rustls_tls()
        .https_only(true);
    if let Some(idle) = idle {
        builder = builder.pool_idle_timeout(idle);
    }
    builder.build().map_err(|e| format!("h2 client: {e}"))
}

#[derive(Clone)]
struct DialLayer {
    dials: Arc<AtomicUsize>,
}

struct DialService<S> {
    inner: S,
    dials: Arc<AtomicUsize>,
}

impl<S: Clone> Clone for DialService<S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            dials: Arc::clone(&self.dials),
        }
    }
}

impl<S> tower_layer::Layer<S> for DialLayer {
    type Service = DialService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        DialService {
            inner,
            dials: Arc::clone(&self.dials),
        }
    }
}

impl<S, Request> tower_service::Service<Request> for DialService<S>
where
    S: tower_service::Service<Request> + Clone + Send + Sync + 'static,
    S::Future: Send + 'static,
    Request: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        self.dials.fetch_add(1, Ordering::SeqCst);
        self.inner.call(request)
    }
}

fn negotiated_h2(
    captured: &hyper_util::client::legacy::connect::CaptureConnection,
) -> Option<bool> {
    captured
        .connection_metadata()
        .as_ref()
        .map(|info| info.is_negotiated_h2())
}

fn proxied(captured: &hyper_util::client::legacy::connect::CaptureConnection) -> Option<bool> {
    captured
        .connection_metadata()
        .as_ref()
        .map(|info| info.is_proxied())
}

async fn hyper_fetch<C>(
    client: &Client<C, http_body_util::Empty<bytes::Bytes>>,
    url: &str,
) -> Result<
    (
        hyper_util::client::legacy::connect::CaptureConnection,
        Option<SocketAddr>,
        http::Version,
    ),
    String,
>
where
    C: hyper_util::client::legacy::connect::Connect + Clone + Send + Sync + 'static,
{
    let mut request = http::Request::builder()
        .uri(url)
        .body(http_body_util::Empty::<bytes::Bytes>::new())
        .map_err(|e| format!("request: {e}"))?;
    let captured = capture_connection(&mut request);
    let response = client
        .request(request)
        .await
        .map_err(|e| format!("hyper send {url}: {e}"))?;
    let version = response.version();
    let local = response
        .extensions()
        .get::<HttpInfo>()
        .map(HttpInfo::local_addr);
    let bytes = response
        .into_body()
        .collect()
        .await
        .map_err(|e| format!("hyper body: {e}"))?
        .to_bytes();
    if !String::from_utf8_lossy(&bytes).contains(HARNESS_MARKER) {
        return Err(format!("hyper body {bytes:?}"));
    }
    if negotiated_h2(&captured).is_none() {
        return Err("capture was empty after the response".to_string());
    }
    Ok((captured, local, version))
}

struct CleartextH1 {
    addr: SocketAddr,
    accepted: Arc<AtomicUsize>,
    closed: Arc<AtomicUsize>,
    shutdown: tokio::sync::watch::Sender<bool>,
    join: tokio::task::JoinHandle<()>,
}

impl CleartextH1 {
    async fn bind(hold_until: usize) -> Result<Self, String> {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .map_err(|e| format!("bind: {e}"))?;
        let addr = listener.local_addr().map_err(|e| format!("addr: {e}"))?;
        let accepted = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicUsize::new(0));
        let released = Arc::new(AtomicBool::new(hold_until == 0));
        let (shutdown, mut flag) = tokio::sync::watch::channel(false);
        let accepted_task = Arc::clone(&accepted);
        let closed_task = Arc::clone(&closed);
        let join = tokio::spawn(async move {
            loop {
                let incoming = tokio::select! {
                    biased;
                    changed = flag.changed() => {
                        if changed.is_err() || *flag.borrow() {
                            break;
                        }
                        continue;
                    }
                    incoming = listener.accept() => incoming,
                };
                let Ok((mut tcp, _)) = incoming else {
                    break;
                };
                let n = accepted_task.fetch_add(1, Ordering::SeqCst) + 1;
                if hold_until == 0 || n >= hold_until {
                    released.store(true, Ordering::SeqCst);
                }
                let closed_conn = Arc::clone(&closed_task);
                let released = Arc::clone(&released);
                tokio::spawn(async move {
                    let _guard = CloseOnDrop(closed_conn);
                    let _ = tcp.set_nodelay(true);
                    loop {
                        match read_headers(&mut tcp).await {
                            Ok(true) => {}
                            _ => break,
                        }
                        if !released.load(Ordering::SeqCst) {
                            wait_release(&released).await;
                        }
                        let body = HARNESS_MARKER.as_bytes();
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                            body.len()
                        );
                        if tcp.write_all(head.as_bytes()).await.is_err()
                            || tcp.write_all(body).await.is_err()
                        {
                            break;
                        }
                    }
                });
            }
        });
        Ok(CleartextH1 {
            addr,
            accepted,
            closed,
            shutdown,
            join,
        })
    }

    fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }

    fn closed(&self) -> usize {
        self.closed.load(Ordering::SeqCst)
    }
}

impl Drop for CleartextH1 {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        self.join.abort();
    }
}

struct CloseOnDrop(Arc<AtomicUsize>);

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

async fn read_headers(tcp: &mut TcpStream) -> Result<bool, ()> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        let n = tcp.read(&mut tmp).await.map_err(|_| ())?;
        if n == 0 {
            return Ok(false);
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|window| window == b"\r\n\r\n") {
            return Ok(true);
        }
        if buf.len() > 64 * 1024 {
            return Err(());
        }
    }
}

struct H2Listen {
    addr: SocketAddr,
    tcp_accepts: Arc<AtomicUsize>,
    tls_ok: Arc<AtomicUsize>,
    h2_ok: Arc<AtomicUsize>,
    h2_live: Arc<AtomicUsize>,
    shutdown: tokio::sync::watch::Sender<bool>,
    join: tokio::task::JoinHandle<()>,
}

impl H2Listen {
    async fn bind(ca: &HarnessCa, hold_tls_until: usize) -> Result<Self, String> {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .map_err(|e| format!("bind: {e}"))?;
        let addr = listener.local_addr().map_err(|e| format!("addr: {e}"))?;
        let acceptor = tokio_rustls::TlsAcceptor::from(ca.server_config_alpn(&[b"h2"])?);
        let tcp_accepts = Arc::new(AtomicUsize::new(0));
        let tls_ok = Arc::new(AtomicUsize::new(0));
        let h2_ok = Arc::new(AtomicUsize::new(0));
        let h2_live = Arc::new(AtomicUsize::new(0));
        let released = Arc::new(AtomicBool::new(hold_tls_until == 0));
        let (shutdown, mut flag) = tokio::sync::watch::channel(false);
        let tcp_task = Arc::clone(&tcp_accepts);
        let tls_task = Arc::clone(&tls_ok);
        let h2_task = Arc::clone(&h2_ok);
        let live_task = Arc::clone(&h2_live);
        let join = tokio::spawn(async move {
            loop {
                let incoming = tokio::select! {
                    biased;
                    changed = flag.changed() => {
                        if changed.is_err() || *flag.borrow() {
                            break;
                        }
                        continue;
                    }
                    incoming = listener.accept() => incoming,
                };
                let Ok((tcp, _)) = incoming else {
                    break;
                };
                let n = tcp_task.fetch_add(1, Ordering::SeqCst) + 1;
                if hold_tls_until == 0 || n >= hold_tls_until {
                    released.store(true, Ordering::SeqCst);
                }
                let acceptor = acceptor.clone();
                let tls_ok = Arc::clone(&tls_task);
                let h2_ok = Arc::clone(&h2_task);
                let h2_live = Arc::clone(&live_task);
                let released = Arc::clone(&released);
                tokio::spawn(async move {
                    if !released.load(Ordering::SeqCst) {
                        wait_release(&released).await;
                    }
                    let Ok(tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    tls_ok.fetch_add(1, Ordering::SeqCst);
                    let Ok(mut connection) = h2::server::handshake(tls).await else {
                        return;
                    };
                    h2_ok.fetch_add(1, Ordering::SeqCst);
                    h2_live.fetch_add(1, Ordering::SeqCst);
                    let _live = LiveOnDrop(Arc::clone(&h2_live));
                    while let Some(incoming) = connection.accept().await {
                        let Ok((_request, mut respond)) = incoming else {
                            break;
                        };
                        let response = http::Response::builder()
                            .status(http::StatusCode::OK)
                            .header(http::header::CONTENT_LENGTH, HARNESS_MARKER.len())
                            .body(())
                            .unwrap_or_else(|_| http::Response::new(()));
                        let Ok(mut body) = respond.send_response(response, false) else {
                            break;
                        };
                        if body
                            .send_data(bytes::Bytes::from_static(HARNESS_MARKER.as_bytes()), true)
                            .is_err()
                        {
                            break;
                        }
                    }
                });
            }
        });
        Ok(H2Listen {
            addr,
            tcp_accepts,
            tls_ok,
            h2_ok,
            h2_live,
            shutdown,
            join,
        })
    }

    fn tcp_accepts(&self) -> usize {
        self.tcp_accepts.load(Ordering::SeqCst)
    }

    fn tls_ok(&self) -> usize {
        self.tls_ok.load(Ordering::SeqCst)
    }

    fn h2_ok(&self) -> usize {
        self.h2_ok.load(Ordering::SeqCst)
    }

    fn h2_live(&self) -> usize {
        self.h2_live.load(Ordering::SeqCst)
    }
}

impl Drop for H2Listen {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        self.join.abort();
    }
}

struct LiveOnDrop(Arc<AtomicUsize>);

impl Drop for LiveOnDrop {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn wait_release(released: &AtomicBool) {
    let start = std::time::Instant::now();
    while !released.load(Ordering::SeqCst) {
        if start.elapsed() > PEER_WAIT {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_until(mut ready: impl FnMut() -> bool) -> Result<(), String> {
    for _ in 0..40 {
        if ready() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err("timed out waiting for the server".to_string())
}

fn unique_sockets(ports: &[SocketAddr]) -> usize {
    let mut copy = ports.to_vec();
    copy.sort();
    copy.dedup();
    copy.len()
}
