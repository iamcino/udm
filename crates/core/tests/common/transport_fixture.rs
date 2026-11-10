//! Local S0 proof origin. No OS DNS, trust-store, proxy or network mutation.
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
pub struct Barrier {
    release: watch::Sender<u8>,
    pub events: Arc<Mutex<Vec<Event>>>,
}
impl Barrier {
    fn new() -> Self {
        Self {
            release: watch::channel(3).0,
            events: Arc::default(),
        }
    }
    pub fn hold(&self) {
        self.release.send_replace(0);
    }
    pub fn release(&self, stage: u8) {
        self.release.send_replace(stage);
    }
    async fn arrive(&self, mut event: Event, stage: u8, bytes: usize) {
        event.stage = stage;
        event.bytes = bytes;
        self.events.lock().unwrap().push(event);
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
    pub async fn start() -> Self {
        let ca = HarnessCa::generate().unwrap();
        let l4 = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("IPv4 fixture");
        let v4 = l4.local_addr().unwrap();
        let l6 = TcpListener::bind((Ipv6Addr::LOCALHOST, v4.port()))
            .await
            .expect("NOT RUN: IPv6 loopback fixture unavailable");
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
                while let Ok((tcp, _)) = listener.accept().await {
                    let acceptor = acceptor.clone();
                    let gate = gate.clone();
                    connections.spawn(async move {
                        let Ok(tls) = acceptor.accept(tcp).await else {
                            return;
                        };
                        assert_eq!(tls.get_ref().1.alpn_protocol(), Some(b"h2".as_slice()));
                        let mut conn = h2::server::handshake(tls).await.unwrap();
                        let mut streams = tokio::task::JoinSet::new();
                        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
                        while let Some(Ok((request, mut respond))) = conn.accept().await {
                            let request = request.map(|_| ());
                            let (headers, body) = response(&request);
                            let evidence = event(&request, id, family, "h2");
                            let head = request.method() == http::Method::HEAD;
                            let mut send = respond.send_response(headers, head).unwrap();
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
                                    let capacity =
                                        futures_util::future::poll_fn(|cx| send.poll_capacity(cx))
                                            .await;
                                    let Some(Ok(capacity)) = capacity else { return };
                                    let end = end.min(offset + capacity);
                                    if end == offset {
                                        continue;
                                    }
                                    send.send_data(body.slice(offset..end), false).unwrap();
                                    offset = end;
                                    if offset == 1024 {
                                        gate.arrive(evidence.clone(), 2, offset).await;
                                    }
                                }
                                gate.arrive(evidence, 3, total).await;
                                let _ = send.send_data(Bytes::new(), true);
                            });
                        }
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
                        let (headers, body) = response(&request);
                        let gate = gate.clone();
                        streams.spawn(async move {
                            stream.send_response(headers).await.unwrap();
                            if head {
                                stream.finish().await.unwrap();
                                return;
                            }
                            gate.arrive(evidence.clone(), 1, 0).await;
                            let split = body.len().min(1024);
                            stream.send_data(body.slice(..split)).await.unwrap();
                            gate.arrive(evidence.clone(), 2, split).await;
                            if split < body.len() {
                                stream.send_data(body.slice(split..)).await.unwrap();
                            }
                            gate.arrive(evidence, 3, body.len()).await;
                            let _ = stream.finish().await;
                        });
                    }
                });
            }
        }));
        Self {
            ca,
            v4,
            v6,
            h3,
            gates,
            endpoint,
            jobs,
        }
    }
    /// Close one fixture route including its accepted connections and streams.
    pub fn kill_path(&self, index: usize) {
        self.jobs[index].abort();
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
            "src/net/h3.rs",
            "tests/transport_proof_a.rs",
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
            "a":self.v4,"aaaa":self.v6,"h3_v4":self.h3,"ranges":RANGES,
            "certificate_sha256":hex::encode(Sha256::digest(self.ca.ca_der())),
            "failure_schedule":"headers,1024 bytes,body complete; explicit release",
            "source_base":"1c04ec6","source_head":String::from_utf8_lossy(&source.stdout).trim(),"source_files":source_hashes,"binary_sha256":hex::encode(Sha256::digest(std::fs::read(std::env::current_exe().unwrap()).unwrap()))})
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
