//! Local S0 proof origin. No OS DNS, trust-store, proxy or network mutation.
// Integration binaries exercise different fixture controls.
#![allow(dead_code)]
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
use std::time::Duration;

use bytes::Bytes;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::common::dev_tls::HarnessCa;

pub const SIZE: usize = 4 * 65536 + 137;
pub const ETAG: &str = "\"fixed\"";
pub const RANGES: [(usize, usize); 4] = [
    (0, 65535),
    (65536, 131071),
    (131072, 196607),
    (196608, SIZE - 1),
];

pub fn payload() -> Vec<u8> {
    (0..SIZE).map(|i| ((i * 31 + 17) % 251) as u8).collect()
}

#[derive(Clone, Debug)]
pub struct Event {
    pub request: u64,
    pub connection: u64,
    pub uri: String,
    pub family: &'static str,
    pub protocol: &'static str,
    pub range: Option<String>,
    pub if_range: Option<String>,
    pub stage: u8,
    pub bytes: usize,
}

#[derive(Clone, Debug)]
pub struct Accept {
    pub connection: u64,
    pub local: SocketAddr,
    pub peer: SocketAddr,
    pub alpn: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct Barrier {
    release: watch::Sender<u8>,
    pub accepts: Arc<Mutex<Vec<Accept>>>,
    pub closed: Arc<Mutex<Vec<u64>>>,
    pub connections: Arc<Mutex<std::collections::HashMap<u64, tokio::sync::oneshot::Sender<()>>>>,
    pub events: Arc<Mutex<Vec<Event>>>,
    pub fault: Arc<Mutex<Option<&'static str>>>,
    pub at_bytes: Arc<AtomicU64>,
    pub goaway_start: Arc<AtomicU64>,
    pub bad_range_start: Arc<AtomicU64>,
    pub headers: Arc<Mutex<Vec<(String, String)>>>,
    pub redirect: Arc<Mutex<Option<String>>>,
}
impl Barrier {
    fn new() -> Self {
        Self {
            release: watch::channel(3).0,
            accepts: Arc::default(),
            closed: Arc::default(),
            connections: Arc::default(),
            events: Arc::default(),
            fault: Arc::default(),
            at_bytes: Arc::new(AtomicU64::new(1024)),
            goaway_start: Arc::new(AtomicU64::new(u64::MAX)),
            bad_range_start: Arc::new(AtomicU64::new(u64::MAX)),
            headers: Arc::default(),
            redirect: Arc::default(),
        }
    }
    pub fn hold(&self) {
        self.release.send_replace(0);
    }
    pub fn release(&self, stage: u8) {
        self.release.send_replace(stage);
    }
    async fn arrive(&self, mut event: Event, stage: u8, bytes: usize) {
        let small_probe = event.range.as_deref() == Some("bytes=0-0");
        event.stage = stage;
        event.bytes = bytes;
        self.events.lock().unwrap().push(event);
        if small_probe {
            return;
        }
        let mut receiver = self.release.subscribe();
        tokio::time::timeout(Duration::from_secs(30), async {
            while *receiver.borrow_and_update() < stage {
                receiver.changed().await.expect("fixture barrier owner");
            }
        })
        .await
        .expect("NOT RUN: fixture barrier timed out");
    }
}

pub struct Fixture {
    pub ca: HarnessCa,
    pub v4: SocketAddr,
    pub v6: SocketAddr,
    pub h3: SocketAddr,
    pub gates: [Barrier; 3],
    endpoint: quinn::Endpoint,
    jobs: Vec<JoinHandle<()>>,
}

fn response(request: &http::Request<()>) -> (http::Response<()>, Bytes) {
    let range = request.headers().get("range").and_then(|h| h.to_str().ok());
    let (start, end) = range
        .and_then(|v| v.strip_prefix("bytes="))
        .and_then(|v| v.split_once('-'))
        .map(|(a, b)| {
            (
                a.parse::<usize>().unwrap(),
                if b.is_empty() {
                    SIZE - 1
                } else {
                    b.parse::<usize>().unwrap()
                },
            )
        })
        .unwrap_or((0, SIZE - 1));
    assert!(
        start <= end && end < SIZE,
        "fixture accepts only valid ranges"
    );
    let mut builder = http::Response::builder()
        .status(if range.is_some() { 206 } else { 200 })
        .header("content-length", end - start + 1)
        .header("etag", ETAG)
        .header("accept-ranges", "bytes")
        .header("content-encoding", "identity");
    if range.is_some() {
        builder = builder.header("content-range", format!("bytes {start}-{end}/{SIZE}"));
    }
    (
        builder.body(()).unwrap(),
        Bytes::copy_from_slice(&payload()[start..=end]),
    )
}
fn decorate(gate: &Barrier, request: &http::Request<()>, headers: &mut http::Response<()>) {
    for (name, value) in gate.headers.lock().unwrap().iter() {
        headers.headers_mut().insert(
            http::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
    }
    if request.uri().path() == "/redirect.bin" {
        if let Some(location) = gate.redirect.lock().unwrap().as_ref() {
            *headers.status_mut() = http::StatusCode::FOUND;
            headers
                .headers_mut()
                .insert("location", location.parse().unwrap());
        }
    }
}
fn event(
    request: &http::Request<()>,
    id: u64,
    family: &'static str,
    protocol: &'static str,
) -> Event {
    Event {
        request: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        connection: id,
        uri: request.uri().to_string(),
        family,
        protocol,
        range: request
            .headers()
            .get("range")
            .map(|h| h.to_str().unwrap().to_owned()),
        if_range: request
            .headers()
            .get("if-range")
            .map(|h| h.to_str().unwrap().to_owned()),
        stage: 0,
        bytes: 0,
    }
}

impl Fixture {
    pub async fn start() -> Option<Self> {
        let ca = HarnessCa::generate().unwrap();
        let mut pair = None;
        for _ in 0..32 {
            let l4 = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .await
                .expect("IPv4 fixture");
            let port = l4.local_addr().unwrap().port();
            match TcpListener::bind((Ipv6Addr::LOCALHOST, port)).await {
                Ok(l6) => {
                    pair = Some((l4, l6));
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                    // IPv4's ephemeral choice can still be busy on IPv6.
                    drop(l4);
                    tokio::task::yield_now().await;
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::AddrNotAvailable
                        // Linux, macOS/BSD, and Windows EAFNOSUPPORT.
                        || matches!(error.raw_os_error(), Some(97 | 47 | 10047)) =>
                {
                    eprintln!("SKIP: IPv6 loopback fixture unavailable: {error}");
                    return None;
                }
                Err(error) => panic!("IPv6 fixture bind failed: {error}"),
            }
        }
        let (l4, l6) = pair.unwrap_or_else(|| {
            panic!("paired loopback fixture bind exhausted 32 AddrInUse attempts")
        });
        let v4 = l4.local_addr().unwrap();
        let v6 = l6.local_addr().unwrap();
        let endpoint = quinn::Endpoint::server(
            ca.quic_server_config().unwrap(),
            (Ipv4Addr::LOCALHOST, 0).into(),
        )
        .unwrap();
        let h3 = endpoint.local_addr().unwrap();
        let gates = [Barrier::new(), Barrier::new(), Barrier::new()];
        let acceptor = tokio_rustls::TlsAcceptor::from(ca.server_config_alpn(&[b"h2"]).unwrap());
        let mut jobs = Vec::new();
        for (listener, family, gate) in [(l4, "v4", gates[0].clone()), (l6, "v6", gates[1].clone())]
        {
            let acceptor = acceptor.clone();
            jobs.push(tokio::spawn(async move {
                let mut connections = tokio::task::JoinSet::new();
                while let Ok((tcp, peer)) = listener.accept().await {
                    let local = tcp.local_addr().unwrap();
                    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
                    let acceptor = acceptor.clone();
                    let gate = gate.clone();
                    connections.spawn(async move {
                        let Ok(tls) = acceptor.accept(tcp).await else {
                            return;
                        };
                        assert_eq!(tls.get_ref().1.alpn_protocol(), Some(b"h2".as_slice()));
                        gate.accepts.lock().unwrap().push(Accept {
                            connection: id,
                            local,
                            peer,
                            alpn: tls.get_ref().1.alpn_protocol().unwrap().to_vec(),
                        });
                        let (kill, killed) = tokio::sync::oneshot::channel();
                        gate.connections.lock().unwrap().insert(id, kill);
                        let serve = async {
                            let mut conn = h2::server::handshake(tls).await.unwrap();
                            let mut streams = tokio::task::JoinSet::new();
                            while let Some(Ok((request, mut respond))) = conn.accept().await {
                                let request = request.map(|_| ());
                                let (mut headers, mut body) = response(&request);
                                decorate(&gate, &request, &mut headers);
                                match *gate.fault.lock().unwrap() {
                                    Some("no-validator") => {
                                        headers.headers_mut().remove("etag");
                                    }
                                    Some("weak-bound") => {
                                        headers
                                            .headers_mut()
                                            .insert("etag", "W/\"fixed\"".parse().unwrap());
                                    }
                                    Some("vary-bound") => {
                                        headers
                                            .headers_mut()
                                            .insert("vary", "Cookie".parse().unwrap());
                                    }
                                    _ => {}
                                }
                                let small = request
                                    .headers()
                                    .get("range")
                                    .is_some_and(|r| r == "bytes=0-0");
                                if !small && request.method() != http::Method::HEAD {
                                    let target = gate.bad_range_start.load(Ordering::SeqCst);
                                    let targeted = target == u64::MAX
                                        || request
                                            .headers()
                                            .get("range")
                                            .and_then(|v| v.to_str().ok())
                                            .is_some_and(|v| {
                                                v.starts_with(&format!("bytes={target}-"))
                                            });
                                    let fault = if targeted {
                                        *gate.fault.lock().unwrap()
                                    } else {
                                        None
                                    };
                                    match fault {
                                        Some("redirect")
                                            if request.uri().path() == "/proof.bin" =>
                                        {
                                            *headers.status_mut() = http::StatusCode::FOUND;
                                            headers.headers_mut().insert(
                                                "location",
                                                "/changed.bin".parse().unwrap(),
                                            );
                                        }
                                        Some("etag") => {
                                            headers
                                                .headers_mut()
                                                .insert("etag", "\"changed\"".parse().unwrap());
                                        }
                                        Some("weak") => {
                                            headers
                                                .headers_mut()
                                                .insert("etag", "W/\"fixed\"".parse().unwrap());
                                        }
                                        Some("missing") => {
                                            headers.headers_mut().remove("etag");
                                        }
                                        Some("total") => {
                                            let range = headers.headers()["content-range"]
                                                .to_str()
                                                .unwrap()
                                                .rsplit_once('/')
                                                .unwrap()
                                                .0;
                                            let changed =
                                                format!("{range}/{}", SIZE + 1).parse().unwrap();
                                            headers.headers_mut().insert("content-range", changed);
                                        }
                                        Some("interval") => {
                                            headers.headers_mut().insert(
                                                "content-range",
                                                format!("bytes 1-1/{SIZE}").parse().unwrap(),
                                            );
                                        }
                                        Some("short") => {
                                            body = body.slice(..body.len() / 2);
                                        }
                                        Some("excess") => {
                                            let mut b = body.to_vec();
                                            b.push(42);
                                            body = b.into();
                                        }
                                        Some("200") => {
                                            *headers.status_mut() = http::StatusCode::OK;
                                        }
                                        Some("416") => {
                                            *headers.status_mut() =
                                                http::StatusCode::RANGE_NOT_SATISFIABLE;
                                        }
                                        Some("coding") => {
                                            headers.headers_mut().insert(
                                                "content-encoding",
                                                "gzip".parse().unwrap(),
                                            );
                                        }
                                        Some("vary") => {
                                            headers
                                                .headers_mut()
                                                .insert("vary", "Cookie".parse().unwrap());
                                        }
                                        _ => {}
                                    }
                                }
                                let evidence = event(&request, id, family, "h2");
                                let head = request.method() == http::Method::HEAD;
                                let Ok(mut send) = respond.send_response(headers, head) else {
                                    // Cancellation or an injected malformed response can
                                    // reset a stream while the fixture prepares headers.
                                    continue;
                                };
                                let targeted = request
                                    .headers()
                                    .get("range")
                                    .and_then(|r| r.to_str().ok())
                                    .and_then(|r| r.strip_prefix("bytes="))
                                    .and_then(|r| r.split_once('-'))
                                    .and_then(|(start, _)| start.parse::<u64>().ok())
                                    .is_some_and(|start| {
                                        !small
                                            && gate
                                                .goaway_start
                                                .compare_exchange(
                                                    start,
                                                    u64::MAX,
                                                    Ordering::SeqCst,
                                                    Ordering::SeqCst,
                                                )
                                                .is_ok()
                                    });
                                let goaway = !small
                                    && (targeted || *gate.fault.lock().unwrap() == Some("goaway"));
                                if goaway {
                                    conn.graceful_shutdown();
                                }
                                if head {
                                    continue;
                                }
                                let gate = gate.clone();
                                streams.spawn(async move {
                                    gate.arrive(evidence.clone(), 1, 0).await;
                                    let total = body.len();
                                    let mut offset = 0;
                                    while offset < total {
                                        let end = (offset + 1024).min(total);
                                        send.reserve_capacity(end - offset);
                                        let capacity = futures_util::future::poll_fn(|cx| {
                                            send.poll_capacity(cx)
                                        })
                                        .await;
                                        let Some(Ok(capacity)) = capacity else { return };
                                        let end = end.min(offset + capacity);
                                        if end == offset {
                                            continue;
                                        }
                                        if send.send_data(body.slice(offset..end), false).is_err() {
                                            return;
                                        }
                                        offset = end;
                                        if goaway && offset >= 1024 {
                                            send.send_reset(h2::Reason::CANCEL);
                                            return;
                                        }
                                        if offset == gate.at_bytes.load(Ordering::Relaxed) as usize
                                        {
                                            gate.arrive(evidence.clone(), 2, offset).await;
                                        }
                                    }
                                    gate.arrive(evidence, 3, total).await;
                                    let _ = send.send_data(Bytes::new(), true);
                                });
                            }
                        };
                        tokio::select! { _ = serve => {}, _ = killed => {} }
                        gate.connections.lock().unwrap().remove(&id);
                        gate.closed.lock().unwrap().push(id);
                    });
                }
            }));
        }
        let server = endpoint.clone();
        let gate = gates[2].clone();
        jobs.push(tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            while let Some(incoming) = server.accept().await {
                let gate = gate.clone();
                connections.spawn(async move {
                    let Ok(conn) = incoming.await else { return };
                    let mut conn = h3::server::Connection::new(h3_quinn::Connection::new(conn))
                        .await
                        .unwrap();
                    let mut streams = tokio::task::JoinSet::new();
                    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
                    while let Ok(Some(resolver)) = conn.accept().await {
                        let (request, mut stream) = resolver.resolve_request().await.unwrap();

                        let evidence = event(&request, id, "v4", "h3");
                        let head = request.method() == http::Method::HEAD;
                        let (mut headers, body) = response(&request);
                        decorate(&gate, &request, &mut headers);
                        let small = request
                            .headers()
                            .get("range")
                            .is_some_and(|r| r == "bytes=0-0");
                        if !small && !head {
                            match *gate.fault.lock().unwrap() {
                                Some("etag") => {
                                    headers
                                        .headers_mut()
                                        .insert("etag", "\"changed\"".parse().unwrap());
                                }
                                Some("missing") => {
                                    headers.headers_mut().remove("etag");
                                }
                                Some("coding") => {
                                    headers
                                        .headers_mut()
                                        .insert("content-encoding", "gzip".parse().unwrap());
                                }
                                Some("total") => {
                                    let range = headers.headers()["content-range"]
                                        .to_str()
                                        .unwrap()
                                        .rsplit_once('/')
                                        .unwrap()
                                        .0;
                                    let changed = format!("{range}/{}", SIZE + 1).parse().unwrap();
                                    headers.headers_mut().insert("content-range", changed);
                                }
                                Some("200") => {
                                    *headers.status_mut() = http::StatusCode::OK;
                                }
                                Some("redirect") if request.uri().path() == "/proof.bin" => {
                                    *headers.status_mut() = http::StatusCode::FOUND;
                                    headers
                                        .headers_mut()
                                        .insert("location", "/changed.bin".parse().unwrap());
                                }
                                _ => {}
                            }
                        }
                        gate.events.lock().unwrap().push(evidence.clone());
                        let gate = gate.clone();
                        streams.spawn(async move {
                            if stream.send_response(headers).await.is_err() {
                                return;
                            }
                            if head {
                                let _ = stream.finish().await;
                                return;
                            }
                            gate.arrive(evidence.clone(), 1, 0).await;
                            let split = body
                                .len()
                                .min(gate.at_bytes.load(Ordering::Relaxed) as usize);
                            if stream.send_data(body.slice(..split)).await.is_err() {
                                return;
                            }
                            gate.arrive(evidence.clone(), 2, split).await;
                            if split < body.len() {
                                if stream.send_data(body.slice(split..)).await.is_err() {
                                    return;
                                }
                            }
                            gate.arrive(evidence, 3, body.len()).await;
                            let _ = stream.finish().await;
                        });
                    }
                });
            }
        }));
        Some(Self {
            ca,
            v4,
            v6,
            h3,
            gates,
            endpoint,
            jobs,
        })
    }
    /// Close one fixture route including its accepted connections and streams.
    pub fn kill_path(&self, index: usize) {
        self.jobs[index].abort();
    }
    #[allow(dead_code)] // Shared fixture: only S2 exercises mixed transport loss.
    pub fn close_h3(&self) {
        self.endpoint.close(0u32.into(), b"S2 transport loss");
    }
    pub fn url(&self) -> String {
        format!("https://localhost:{}/proof.bin", self.v4.port())
    }
    /// Explicit test DNS answers retain the same logical host, Host and SNI.
    pub fn client(&self, peer: SocketAddr) -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .http2_prior_knowledge()
            .add_root_certificate(reqwest::Certificate::from_der(self.ca.ca_der()).unwrap())
            .resolve_to_addrs("localhost", &[peer])
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap()
    }
    pub fn manifest(&self) -> serde_json::Value {
        let source = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .unwrap();
        let source_hashes: Vec<_> = [
            "src/engine/task.rs",
            "src/engine/mod.rs",
            "src/engine/proof.rs",
            "src/net/stack.rs",
            "src/net/lane.rs",
            "src/net/http12.rs",
            "src/config.rs",
            "src/store/records.rs",
            "src/net/h3.rs",
            "tests/transport_proof_a.rs",
            "tests/transport_s1.rs",
            "tests/transport_s2.rs",
            "tests/transport_s3.rs",
            "tests/common/transport_fixture.rs",
        ]
        .into_iter()
        .map(|path| {
            let bytes =
                std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(path)).unwrap();
            (path, hex::encode(Sha256::digest(bytes)))
        })
        .collect();
        serde_json::json!({"seed":17,"size":SIZE,"sha256":hex::encode(Sha256::digest(payload())),
            "etag":ETAG,"coding":"identity","allowed_vary":[],"logical_url":self.url(),
            "a":self.v4,"aaaa":self.v6,"h3_v4":self.h3,"fixture_control_ranges":RANGES,
            "four_segment_planner_ranges":[[0,65569],[65570,131139],[131140,196709],[196710,262280]],
            "certificate_sha256":hex::encode(Sha256::digest(self.ca.ca_der())),
            "failure_schedule":"headers,1024 bytes,body complete; explicit release",
            "fixture_origin_base":"08b2607","source_head":String::from_utf8_lossy(&source.stdout).trim(),"source_files":source_hashes,"binary_sha256":hex::encode(Sha256::digest(std::fs::read(std::env::current_exe().unwrap()).unwrap()))})
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.endpoint.close(0u32.into(), b"fixture done");
        for job in &self.jobs {
            job.abort();
        }
    }
}
