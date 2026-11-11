use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType,
};
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair as _};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::update_manifest::ManifestFile;
use crate::update_pin;

const ISSUED: i64 = 1_700_000_000;
const NOW: i64 = ISSUED + 86_400;

#[derive(Clone)]
enum Answer {
    Drop,
    Status(u16),
    Body {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    Redirect(String),
    Pause {
        body: Vec<u8>,
        started: Arc<AtomicBool>,
    },
}

#[derive(Clone, Debug)]
struct Hit {
    path: String,
    query: String,
    range: Option<String>,
}

type Responder = Arc<dyn Fn(&Hit) -> Answer + Send + Sync>;

struct Loop {
    addr: SocketAddr,
    log: Arc<Mutex<Vec<Hit>>>,
    slot: Arc<Mutex<Responder>>,
    shutdown: tokio::sync::watch::Sender<bool>,
    join: tokio::task::JoinHandle<()>,
}

impl Loop {
    fn set(&self, responder: Responder) {
        *self.slot.lock().unwrap() = responder;
    }
}

impl Drop for Loop {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        self.join.abort();
    }
}

struct Certs {
    client: ClientConfig,
    server: Arc<ServerConfig>,
}

fn install_ring() {
    tls::install_default_provider();
}

fn fixture_certs() -> Certs {
    install_ring();
    issued("UDM U3 fixture CA")
}

fn pin_chained_certs() -> Certs {
    install_ring();
    issued(update_pin::publisher_public_key_hex())
}

fn issued(ca_common_name: &str) -> Certs {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let mut ca_name = DistinguishedName::new();
    ca_name.push(DnType::CommonName, ca_common_name);
    ca_params.distinguished_name = ca_name;
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let server_key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.subject_alt_names = vec![SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST))];
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, "127.0.0.1");
    params.distinguished_name = name;
    let server_cert = params.signed_by(&server_key, &ca_cert, &ca_key).unwrap();
    Certs {
        client: client_trusting(ca_cert.der()),
        server: server_config(&server_cert, &server_key),
    }
}

fn client_trusting(ca: &CertificateDer<'_>) -> ClientConfig {
    let mut roots = RootCertStore::empty();
    roots.add(ca.clone()).unwrap();
    let mut config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    config
}

fn server_config(cert: &rcgen::Certificate, key: &KeyPair) -> Arc<ServerConfig> {
    let private = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
    let mut config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], private)
            .unwrap();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(config)
}

async fn spawn(server: Arc<ServerConfig>) -> Loop {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let acceptor = TlsAcceptor::from(server);
    let log = Arc::new(Mutex::new(Vec::new()));
    let slot: Arc<Mutex<Responder>> = Arc::new(Mutex::new(Arc::new(|_| Answer::Status(404))));
    let (shutdown, mut flag) = tokio::sync::watch::channel(false);
    let log_task = Arc::clone(&log);
    let slot_task = Arc::clone(&slot);
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
            let Ok((tcp, _)) = accepted else { break };
            let acceptor = acceptor.clone();
            let log_task = Arc::clone(&log_task);
            let slot_task = Arc::clone(&slot_task);
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let Ok(hit) = read_hit(&mut tls).await else {
                    return;
                };
                log_task.lock().unwrap().push(hit.clone());
                let responder = Arc::clone(&slot_task.lock().unwrap());
                let answer = responder(&hit);
                let _ = write_answer(&mut tls, answer).await;
            });
        }
    });
    Loop {
        addr,
        log,
        slot,
        shutdown,
        join,
    }
}

async fn read_hit(tls: &mut tokio_rustls::server::TlsStream<TcpStream>) -> Result<Hit, ()> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        let n = tls.read(&mut byte).await.map_err(|_| ())?;
        if n == 0 {
            return Err(());
        }
        buf.push(byte[0]);
        if buf.len() > 16384 {
            return Err(());
        }
    }
    parse_hit(&buf).ok_or(())
}

fn parse_hit(buf: &[u8]) -> Option<Hit> {
    let text = std::str::from_utf8(buf).ok()?;
    let mut lines = text.split("\r\n");
    let target = lines.next()?.split(' ').nth(1)?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut range = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("range") {
            range = Some(value.trim().to_owned());
        }
    }
    Some(Hit {
        path: path.to_owned(),
        query: query.to_owned(),
        range,
    })
}

async fn write_answer(
    tls: &mut tokio_rustls::server::TlsStream<TcpStream>,
    answer: Answer,
) -> Result<(), ()> {
    match answer {
        Answer::Drop => Ok(()),
        Answer::Status(status) => write_bytes(tls, status, &[], Vec::new()).await,
        Answer::Body {
            status,
            headers,
            body,
        } => write_bytes(tls, status, &headers, body).await,
        Answer::Redirect(location) => {
            write_bytes(tls, 302, &[("Location".to_owned(), location)], Vec::new()).await
        }
        Answer::Pause { body, started } => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            tls.write_all(head.as_bytes()).await.map_err(|_| ())?;
            if !body.is_empty() {
                tls.write_all(&body[..1]).await.map_err(|_| ())?;
                tls.flush().await.map_err(|_| ())?;
            }
            started.store(true, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(())
        }
    }
}

async fn write_bytes(
    tls: &mut tokio_rustls::server::TlsStream<TcpStream>,
    status: u16,
    headers: &[(String, String)],
    body: Vec<u8>,
) -> Result<(), ()> {
    let mut head = format!(
        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    tls.write_all(head.as_bytes()).await.map_err(|_| ())?;
    if !body.is_empty() {
        tls.write_all(&body).await.map_err(|_| ())?;
    }
    let _ = tls.shutdown().await;
    Ok(())
}

struct Publisher {
    trust: TrustRoot,
    key: Ed25519KeyPair,
}

fn publisher() -> Publisher {
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
    let key = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
    let mut public_key = [0u8; 32];
    public_key.copy_from_slice(key.public_key().as_ref());
    Publisher {
        trust: TrustRoot {
            public_key,
            pin: hex::encode(Sha256::digest(public_key)),
        },
        key,
    }
}

struct Release {
    json: Vec<u8>,
    zip: Vec<u8>,
}

fn release_at(publisher: &Publisher, port: u16, files: &[(&str, &[u8])]) -> Release {
    let zip = zip_archive(files, false);
    let url = format!(
        "https://127.0.0.1:{port}/payload.zip?sha256={}",
        sha256_bytes(&zip)
    );
    Release {
        json: signed_json(publisher, &url, files, None),
        zip,
    }
}

fn signed_json(
    publisher: &Publisher,
    url: &str,
    files: &[(&str, &[u8])],
    member_override: Option<(&str, String, u64)>,
) -> Vec<u8> {
    let manifest_files: Vec<ManifestFile> = files
        .iter()
        .map(|(path, bytes)| {
            let (sha256, size) = member_override
                .as_ref()
                .filter(|(name, _, _)| *name == *path)
                .map(|(_, sha, size)| (sha.clone(), *size))
                .unwrap_or_else(|| (sha256_bytes(bytes), bytes.len() as u64));
            ManifestFile {
                path: (*path).to_owned(),
                sha256,
                size,
            }
        })
        .collect();
    let manifest = Manifest {
        version: "0.2.0".to_owned(),
        manifest_epoch: 7,
        issued_at: ISSUED,
        valid_until: ISSUED + 7 * 86_400,
        files: manifest_files,
        min_version: "0.1.0".to_owned(),
        urls: vec![url.to_owned()],
        signatures: Vec::new(),
    };
    let sig = publisher.key.sign(&manifest.canonical_bytes());
    let signature = hex::encode(sig.as_ref());
    let mut bytes = manifest.canonical_bytes();
    bytes.pop();
    bytes.extend(
        format!(
            ",\"signatures\":[{{\"key_id\":\"{}\",\"signature\":\"{signature}\"}}]}}",
            publisher.trust.pin
        )
        .into_bytes(),
    );
    bytes
}

fn zip_archive(files: &[(&str, &[u8])], deflate: bool) -> Vec<u8> {
    let mut locals = Vec::new();
    let mut central = Vec::new();
    let mut offset = 0u32;
    for (name, data) in files {
        let name_bytes = name.as_bytes();
        let (method, payload) = if deflate {
            let mut encoder =
                flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(data).unwrap();
            (8u16, encoder.finish().unwrap())
        } else {
            (0u16, data.to_vec())
        };
        let mut local = Vec::new();
        push_u32(&mut local, 0x0403_4b50);
        push_u16(&mut local, 20);
        push_u16(&mut local, 0);
        push_u16(&mut local, method);
        push_u16(&mut local, 0);
        push_u16(&mut local, 0);
        push_u32(&mut local, 0);
        push_u32(&mut local, payload.len() as u32);
        push_u32(&mut local, data.len() as u32);
        push_u16(&mut local, name_bytes.len() as u16);
        push_u16(&mut local, 0);
        local.extend_from_slice(name_bytes);
        local.extend_from_slice(&payload);
        let mut entry = Vec::new();
        push_u32(&mut entry, 0x0201_4b50);
        push_u16(&mut entry, 20);
        push_u16(&mut entry, 20);
        push_u16(&mut entry, 0);
        push_u16(&mut entry, method);
        push_u16(&mut entry, 0);
        push_u16(&mut entry, 0);
        push_u32(&mut entry, 0);
        push_u32(&mut entry, payload.len() as u32);
        push_u32(&mut entry, data.len() as u32);
        push_u16(&mut entry, name_bytes.len() as u16);
        push_u16(&mut entry, 0);
        push_u16(&mut entry, 0);
        push_u16(&mut entry, 0);
        push_u16(&mut entry, 0);
        push_u32(&mut entry, 0);
        push_u32(&mut entry, offset);
        entry.extend_from_slice(name_bytes);
        offset += local.len() as u32;
        locals.extend_from_slice(&local);
        central.extend_from_slice(&entry);
    }
    let cd_offset = locals.len() as u32;
    let cd_size = central.len() as u32;
    let mut out = locals;
    out.extend_from_slice(&central);
    push_u32(&mut out, 0x0605_4b50);
    push_u16(&mut out, 0);
    push_u16(&mut out, 0);
    push_u16(&mut out, files.len() as u16);
    push_u16(&mut out, files.len() as u16);
    push_u32(&mut out, cd_size);
    push_u32(&mut out, cd_offset);
    push_u16(&mut out, 0);
    out
}

fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn serve_release(json: Vec<u8>, zip: Vec<u8>) -> Responder {
    Arc::new(move |hit: &Hit| body_for(hit, &json, &zip))
}

fn body_for(hit: &Hit, json: &[u8], zip: &[u8]) -> Answer {
    if hit.path == "/payload.zip" {
        if let Some(range) = &hit.range {
            let start: usize = range
                .trim()
                .trim_start_matches("bytes=")
                .split('-')
                .next()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0);
            let start = start.min(zip.len());
            let body = zip[start..].to_vec();
            let end = zip.len().saturating_sub(1);
            return Answer::Body {
                status: 206,
                headers: vec![(
                    "Content-Range".to_owned(),
                    format!("bytes {start}-{end}/{}", zip.len()),
                )],
                body,
            };
        }
        return Answer::Body {
            status: 200,
            headers: Vec::new(),
            body: zip.to_vec(),
        };
    }
    Answer::Body {
        status: 200,
        headers: Vec::new(),
        body: json.to_vec(),
    }
}

fn counts(log: &Mutex<Vec<Hit>>, prefix: &str) -> usize {
    log.lock()
        .unwrap()
        .iter()
        .filter(|hit| hit.path.starts_with(prefix))
        .count()
}

fn open_recorded(
    origins: [Origin; 2],
    tls: ClientConfig,
    trust: TrustRoot,
) -> (FetchSession, Arc<Mutex<Vec<Duration>>>) {
    let (pause, record) = Pause::recorded();
    (
        FetchSession::build(origins, tls, trust, pause, false).unwrap(),
        record,
    )
}

fn origins_for(primary: SocketAddr, secondary: SocketAddr) -> [Origin; 2] {
    [
        Origin::new(Url::parse(&format!("https://127.0.0.1:{}/manifest", primary.port())).unwrap())
            .unwrap(),
        Origin::new(
            Url::parse(&format!("https://127.0.0.1:{}/manifest", secondary.port())).unwrap(),
        )
        .unwrap(),
    ]
}

async fn run_fetch(
    session: &FetchSession,
    root: &std::path::Path,
) -> Result<VerifiedPayload, FetchError> {
    let epoch = root.join("epoch.json");
    store_epoch(&epoch, 6).unwrap();
    session
        .fetch(FetchInput {
            installed_version: "0.1.5",
            now_unix: NOW,
            epoch_path: &epoch,
            dest_dir: &root.join("out"),
            cancel: &CancellationToken::new(),
        })
        .await
}

fn manifest_hits(log: &Mutex<Vec<Hit>>) -> Vec<Hit> {
    log.lock()
        .unwrap()
        .iter()
        .filter(|hit| hit.path == "/manifest")
        .cloned()
        .collect()
}

#[test]
fn deflate_member_round_trips() {
    let files = [("udm.exe", b"hello-udm".as_slice())];
    let members = read_zip(&zip_archive(&files, true)).unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].name, "udm.exe");
    assert_eq!(members[0].bytes, b"hello-udm");
}

#[tokio::test]
async fn manifest_refusal_prevents_any_payload_request() {
    let certs = fixture_certs();
    let trusted = publisher();
    let other = publisher();
    let primary = spawn(Arc::clone(&certs.server)).await;
    let secondary = spawn(certs.server).await;
    let bad = signed_json(
        &other,
        &format!(
            "https://127.0.0.1:{}/payload.zip?sha256={}",
            primary.addr.port(),
            "ab".repeat(32)
        ),
        &[("udm.exe", b"hello-udm".as_slice())],
        None,
    );
    let for_primary = bad.clone();
    primary.set(Arc::new(move |_| Answer::Body {
        status: 200,
        headers: Vec::new(),
        body: for_primary.clone(),
    }));
    secondary.set(Arc::new(move |_| Answer::Body {
        status: 200,
        headers: Vec::new(),
        body: bad.clone(),
    }));
    let (session, _) = open_recorded(
        origins_for(primary.addr, secondary.addr),
        certs.client,
        trusted.trust,
    );
    let root = tempfile::tempdir().unwrap();
    let err = run_fetch(&session, root.path()).await.unwrap_err();
    assert!(matches!(err, FetchError::OriginsExhausted(_)));
    assert_eq!(counts(&primary.log, "/payload"), 0);
    assert_eq!(counts(&secondary.log, "/payload"), 0);
    assert!(counts(&primary.log, "/manifest") >= 1);
}

#[tokio::test]
async fn fallback_after_transport_failure_and_remembers_the_working_host() {
    let certs = fixture_certs();
    let publisher = publisher();
    let primary = spawn(Arc::clone(&certs.server)).await;
    let secondary = spawn(certs.server).await;
    let release = release_at(
        &publisher,
        secondary.addr.port(),
        &[("udm.exe", b"hello-udm".as_slice())],
    );
    primary.set(Arc::new(|_| Answer::Drop));
    secondary.set(serve_release(release.json, release.zip));
    let (session, pauses) = open_recorded(
        origins_for(primary.addr, secondary.addr),
        certs.client,
        publisher.trust.clone(),
    );
    let root = tempfile::tempdir().unwrap();
    let got = run_fetch(&session, root.path()).await.unwrap();
    assert_eq!(got.files.len(), 1);
    assert_eq!(counts(&primary.log, "/manifest"), 3);
    assert_eq!(counts(&secondary.log, "/manifest"), 1);
    assert_eq!(counts(&secondary.log, "/payload"), 1);
    assert_eq!(
        pauses.lock().unwrap().as_slice(),
        [Duration::from_secs(1), Duration::from_secs(3)]
    );
    let before = counts(&primary.log, "/manifest");
    let root2 = tempfile::tempdir().unwrap();
    run_fetch(&session, root2.path()).await.unwrap();
    assert_eq!(counts(&primary.log, "/manifest"), before);
    assert!(counts(&secondary.log, "/manifest") >= 2);
}

#[tokio::test]
async fn fallback_after_refused_manifest() {
    let certs = fixture_certs();
    let trusted = publisher();
    let other = publisher();
    let primary = spawn(Arc::clone(&certs.server)).await;
    let secondary = spawn(certs.server).await;
    let release = release_at(
        &trusted,
        secondary.addr.port(),
        &[("udm.exe", b"hello-udm".as_slice())],
    );
    let bad = signed_json(
        &other,
        &format!(
            "https://127.0.0.1:{}/payload.zip?sha256={}",
            primary.addr.port(),
            "ab".repeat(32)
        ),
        &[("udm.exe", b"hello-udm".as_slice())],
        None,
    );
    primary.set(Arc::new(move |_| Answer::Body {
        status: 200,
        headers: Vec::new(),
        body: bad.clone(),
    }));
    secondary.set(serve_release(release.json, release.zip));
    let (session, _) = open_recorded(
        origins_for(primary.addr, secondary.addr),
        certs.client,
        trusted.trust,
    );
    let root = tempfile::tempdir().unwrap();
    run_fetch(&session, root.path()).await.unwrap();
    assert_eq!(counts(&primary.log, "/manifest"), 1);
    assert_eq!(counts(&primary.log, "/payload"), 0);
    assert_eq!(counts(&secondary.log, "/payload"), 1);
}

#[tokio::test]
async fn epoch_is_durable_before_the_first_payload_byte() {
    let certs = fixture_certs();
    let publisher = publisher();
    let primary = spawn(Arc::clone(&certs.server)).await;
    let secondary = spawn(certs.server).await;
    let release = release_at(
        &publisher,
        primary.addr.port(),
        &[("udm.exe", b"hello-udm".as_slice())],
    );
    let seen = Arc::new(Mutex::new(None));
    let epoch_slot = Arc::new(Mutex::new(std::path::PathBuf::new()));
    let seen_task = Arc::clone(&seen);
    let epoch_task = Arc::clone(&epoch_slot);
    let json = release.json.clone();
    let zip = release.zip.clone();
    primary.set(Arc::new(move |hit: &Hit| {
        if hit.path == "/payload.zip" {
            let path = epoch_task.lock().unwrap().clone();
            *seen_task.lock().unwrap() = Some(load_epoch(&path).unwrap());
        }
        body_for(hit, &json, &zip)
    }));
    secondary.set(Arc::new(|_| Answer::Drop));
    let (session, _) = open_recorded(
        origins_for(primary.addr, secondary.addr),
        certs.client,
        publisher.trust,
    );
    let root = tempfile::tempdir().unwrap();
    *epoch_slot.lock().unwrap() = root.path().join("epoch.json");
    run_fetch(&session, root.path()).await.unwrap();
    assert_eq!(*seen.lock().unwrap(), Some(7));
    assert_eq!(load_epoch(&root.path().join("epoch.json")).unwrap(), 7);
}

#[tokio::test]
async fn interactive_manifest_get_carries_a_nonce() {
    let certs = fixture_certs();
    let publisher = publisher();
    let primary = spawn(Arc::clone(&certs.server)).await;
    let secondary = spawn(certs.server).await;
    let release = release_at(
        &publisher,
        primary.addr.port(),
        &[("udm.exe", b"hello-udm".as_slice())],
    );
    primary.set(serve_release(release.json, release.zip));
    secondary.set(Arc::new(|_| Answer::Drop));
    let (session, _) = open_recorded(
        origins_for(primary.addr, secondary.addr),
        certs.client,
        publisher.trust,
    );
    let root = tempfile::tempdir().unwrap();
    run_fetch(&session, root.path()).await.unwrap();
    let manifests = manifest_hits(&primary.log);
    assert_eq!(manifests.len(), 1);
    let nonce = manifests[0]
        .query
        .split('&')
        .find_map(|pair| pair.strip_prefix("nonce="))
        .unwrap();
    assert_eq!(nonce.len(), 32);
    assert!(nonce.chars().all(|c| c.is_ascii_hexdigit()));
    assert!(hits_of(&primary.log, "/payload")
        .iter()
        .all(|hit| !hit.query.contains("sha256") && !hit.query.contains("nonce")));
}

fn hits_of(log: &Mutex<Vec<Hit>>, prefix: &str) -> Vec<Hit> {
    log.lock()
        .unwrap()
        .iter()
        .filter(|hit| hit.path.starts_with(prefix))
        .cloned()
        .collect()
}

#[tokio::test]
async fn transient_retries_twice_with_backoff() {
    let certs = fixture_certs();
    let publisher = publisher();
    let primary = spawn(Arc::clone(&certs.server)).await;
    let secondary = spawn(certs.server).await;
    let release = release_at(
        &publisher,
        primary.addr.port(),
        &[("udm.exe", b"hello-udm".as_slice())],
    );
    let left = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&left);
    let json = release.json.clone();
    let zip = release.zip.clone();
    primary.set(Arc::new(move |hit: &Hit| {
        if hit.path == "/manifest" && counter.fetch_add(1, Ordering::SeqCst) < 2 {
            return Answer::Drop;
        }
        body_for(hit, &json, &zip)
    }));
    secondary.set(Arc::new(|_| Answer::Drop));
    let (session, pauses) = open_recorded(
        origins_for(primary.addr, secondary.addr),
        certs.client,
        publisher.trust,
    );
    let root = tempfile::tempdir().unwrap();
    run_fetch(&session, root.path()).await.unwrap();
    assert_eq!(left.load(Ordering::SeqCst), 3);
    assert_eq!(
        pauses.lock().unwrap().as_slice(),
        [Duration::from_secs(1), Duration::from_secs(3)]
    );
    assert_eq!(counts(&secondary.log, "/manifest"), 0);
}

#[tokio::test]
async fn http_4xx_is_not_retried() {
    let certs = fixture_certs();
    let publisher = publisher();
    let primary = spawn(Arc::clone(&certs.server)).await;
    let secondary = spawn(certs.server).await;
    let release = release_at(
        &publisher,
        secondary.addr.port(),
        &[("udm.exe", b"hello-udm".as_slice())],
    );
    primary.set(Arc::new(|_| Answer::Status(404)));
    secondary.set(serve_release(release.json, release.zip));
    let (session, pauses) = open_recorded(
        origins_for(primary.addr, secondary.addr),
        certs.client,
        publisher.trust,
    );
    let root = tempfile::tempdir().unwrap();
    run_fetch(&session, root.path()).await.unwrap();
    assert_eq!(counts(&primary.log, "/manifest"), 1);
    assert!(pauses.lock().unwrap().is_empty());
}

#[tokio::test]
async fn http_5xx_is_retried_once() {
    let certs = fixture_certs();
    let publisher = publisher();
    let primary = spawn(Arc::clone(&certs.server)).await;
    let secondary = spawn(certs.server).await;
    let release = release_at(
        &publisher,
        primary.addr.port(),
        &[("udm.exe", b"hello-udm".as_slice())],
    );
    let left = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&left);
    let json = release.json.clone();
    let zip = release.zip.clone();
    primary.set(Arc::new(move |hit: &Hit| {
        if hit.path == "/manifest" && counter.fetch_add(1, Ordering::SeqCst) == 0 {
            return Answer::Status(503);
        }
        body_for(hit, &json, &zip)
    }));
    secondary.set(Arc::new(|_| Answer::Status(500)));
    let (session, _) = open_recorded(
        origins_for(primary.addr, secondary.addr),
        certs.client,
        publisher.trust,
    );
    let root = tempfile::tempdir().unwrap();
    run_fetch(&session, root.path()).await.unwrap();
    assert_eq!(counts(&primary.log, "/manifest"), 2);
    assert_eq!(counts(&secondary.log, "/manifest"), 0);
}

#[tokio::test]
async fn range_206_completes_a_partial() {
    let certs = fixture_certs();
    let publisher = publisher();
    let primary = spawn(Arc::clone(&certs.server)).await;
    let secondary = spawn(certs.server).await;
    let files = [("udm.exe", b"hello-udm-range".as_slice())];
    let release = release_at(&publisher, primary.addr.port(), &files);
    primary.set(serve_release(release.json, release.zip.clone()));
    secondary.set(Arc::new(|_| Answer::Drop));
    let (session, _) = open_recorded(
        origins_for(primary.addr, secondary.addr),
        certs.client,
        publisher.trust,
    );
    let root = tempfile::tempdir().unwrap();
    let out = root.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join(".u3-payload.partial"), &release.zip[..4]).unwrap();
    run_fetch(&session, root.path()).await.unwrap();
    let payload_hits = hits_of(&primary.log, "/payload");
    assert_eq!(payload_hits.len(), 1);
    assert_eq!(payload_hits[0].range.as_deref(), Some("bytes=4-"));
    let text = std::fs::read(out.join("udm.exe")).unwrap();
    assert_eq!(text, b"hello-udm-range");
}

#[tokio::test]
async fn range_ignored_restarts_from_byte_zero() {
    let certs = fixture_certs();
    let publisher = publisher();
    let primary = spawn(Arc::clone(&certs.server)).await;
    let secondary = spawn(certs.server).await;
    let files = [("udm.exe", b"hello-udm-full".as_slice())];
    let release = release_at(&publisher, primary.addr.port(), &files);
    let json = release.json.clone();
    let zip = release.zip.clone();
    primary.set(Arc::new(move |hit: &Hit| {
        if hit.path == "/payload.zip" {
            return Answer::Body {
                status: 200,
                headers: Vec::new(),
                body: zip.clone(),
            };
        }
        Answer::Body {
            status: 200,
            headers: Vec::new(),
            body: json.clone(),
        }
    }));
    secondary.set(Arc::new(|_| Answer::Drop));
    let (session, _) = open_recorded(
        origins_for(primary.addr, secondary.addr),
        certs.client,
        publisher.trust,
    );
    let root = tempfile::tempdir().unwrap();
    let out = root.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join(".u3-payload.partial"), b"PREFIX").unwrap();
    run_fetch(&session, root.path()).await.unwrap();
    assert_eq!(counts(&primary.log, "/payload"), 1);
    assert_eq!(
        hits_of(&primary.log, "/payload")[0].range.as_deref(),
        Some("bytes=6-")
    );
    assert_eq!(
        std::fs::read(out.join("udm.exe")).unwrap(),
        b"hello-udm-full"
    );
    assert!(!out.join(".u3-payload.partial").exists());
}

#[tokio::test]
async fn whole_file_hash_mismatch_discards() {
    let certs = fixture_certs();
    let publisher = publisher();
    let primary = spawn(Arc::clone(&certs.server)).await;
    let secondary = spawn(certs.server).await;
    let files = [("udm.exe", b"hello-udm".as_slice())];
    let zip = zip_archive(&files, false);
    let url = format!(
        "https://127.0.0.1:{}/payload.zip?sha256={}",
        primary.addr.port(),
        "ab".repeat(32)
    );
    let json = signed_json(&publisher, &url, &files, None);
    primary.set(serve_release(json, zip));
    secondary.set(Arc::new(|_| Answer::Drop));
    let (session, _) = open_recorded(
        origins_for(primary.addr, secondary.addr),
        certs.client,
        publisher.trust,
    );
    let root = tempfile::tempdir().unwrap();
    let err = run_fetch(&session, root.path()).await.unwrap_err();
    assert!(matches!(err, FetchError::WholeFileMismatch));
    assert!(counts(&primary.log, "/payload") >= 1);
    assert!(!root.path().join("out").join("udm.exe").exists());
    assert!(!root.path().join("out").join(".u3-payload.partial").exists());
}

#[tokio::test]
async fn per_file_hash_or_size_mismatch_refuses_the_handoff() {
    let certs = fixture_certs();
    let publisher = publisher();
    let primary = spawn(Arc::clone(&certs.server)).await;
    let secondary = spawn(certs.server).await;
    let files = [("udm.exe", b"hello-udm".as_slice())];
    let zip = zip_archive(&files, false);
    let url = format!(
        "https://127.0.0.1:{}/payload.zip?sha256={}",
        primary.addr.port(),
        sha256_bytes(&zip)
    );
    let json = signed_json(
        &publisher,
        &url,
        &files,
        Some(("udm.exe", "ab".repeat(32), b"hello-udm".len() as u64)),
    );
    primary.set(serve_release(json, zip));
    secondary.set(Arc::new(|_| Answer::Drop));
    let (session, _) = open_recorded(
        origins_for(primary.addr, secondary.addr),
        certs.client,
        publisher.trust,
    );
    let root = tempfile::tempdir().unwrap();
    let err = run_fetch(&session, root.path()).await.unwrap_err();
    assert!(matches!(err, FetchError::MemberMismatch { .. }));
    assert!(!root.path().join("out").join("udm.exe").exists());
}

#[tokio::test]
async fn per_file_size_mismatch_refuses_the_handoff() {
    let certs = fixture_certs();
    let publisher = publisher();
    let primary = spawn(Arc::clone(&certs.server)).await;
    let secondary = spawn(certs.server).await;
    let files = [("udm.exe", b"hello-udm".as_slice())];
    let zip = zip_archive(&files, false);
    let url = format!(
        "https://127.0.0.1:{}/payload.zip?sha256={}",
        primary.addr.port(),
        sha256_bytes(&zip)
    );
    let json = signed_json(
        &publisher,
        &url,
        &files,
        Some(("udm.exe", sha256_bytes(b"hello-udm"), 1)),
    );
    primary.set(serve_release(json, zip));
    secondary.set(Arc::new(|_| Answer::Drop));
    let (session, _) = open_recorded(
        origins_for(primary.addr, secondary.addr),
        certs.client,
        publisher.trust,
    );
    let root = tempfile::tempdir().unwrap();
    let err = run_fetch(&session, root.path()).await.unwrap_err();
    assert!(matches!(err, FetchError::MemberMismatch { .. }));
    assert!(!root.path().join("out").join("udm.exe").exists());
}

#[tokio::test]
async fn extracted_file_missing_from_the_manifest_refuses_the_handoff() {
    let certs = fixture_certs();
    let publisher = publisher();
    let primary = spawn(Arc::clone(&certs.server)).await;
    let secondary = spawn(certs.server).await;
    let zip = zip_archive(
        &[
            ("udm.exe", b"hello-udm".as_slice()),
            ("extra.txt", b"nope".as_slice()),
        ],
        false,
    );
    let url = format!(
        "https://127.0.0.1:{}/payload.zip?sha256={}",
        primary.addr.port(),
        sha256_bytes(&zip)
    );
    let json = signed_json(
        &publisher,
        &url,
        &[("udm.exe", b"hello-udm".as_slice())],
        None,
    );
    primary.set(serve_release(json, zip));
    secondary.set(Arc::new(|_| Answer::Drop));
    let (session, _) = open_recorded(
        origins_for(primary.addr, secondary.addr),
        certs.client,
        publisher.trust,
    );
    let root = tempfile::tempdir().unwrap();
    let err = run_fetch(&session, root.path()).await.unwrap_err();
    assert!(matches!(err, FetchError::ExtraMember(_)));
    assert!(!root.path().join("out").join("udm.exe").exists());
    assert!(!root.path().join("out").join("extra.txt").exists());
}

#[tokio::test]
async fn manifest_path_missing_from_the_archive_refuses_the_handoff() {
    let certs = fixture_certs();
    let publisher = publisher();
    let primary = spawn(Arc::clone(&certs.server)).await;
    let secondary = spawn(certs.server).await;
    let zip = zip_archive(&[("udm.exe", b"hello-udm".as_slice())], false);
    let url = format!(
        "https://127.0.0.1:{}/payload.zip?sha256={}",
        primary.addr.port(),
        sha256_bytes(&zip)
    );
    let readme = b"notes".as_slice();
    let json = signed_json(
        &publisher,
        &url,
        &[
            ("udm.exe", b"hello-udm".as_slice()),
            ("assets/readme.txt", readme),
        ],
        None,
    );
    primary.set(serve_release(json, zip));
    secondary.set(Arc::new(|_| Answer::Drop));
    let (session, _) = open_recorded(
        origins_for(primary.addr, secondary.addr),
        certs.client,
        publisher.trust,
    );
    let root = tempfile::tempdir().unwrap();
    let err = run_fetch(&session, root.path()).await.unwrap_err();
    assert!(matches!(err, FetchError::MissingMember(_)));
    assert!(!root.path().join("out").join("udm.exe").exists());
}

#[tokio::test]
async fn pin_is_never_consulted_as_a_tls_trust_root() {
    let chained = pin_chained_certs();
    let server = spawn(chained.server).await;
    server.set(Arc::new(|_| Answer::Body {
        status: 200,
        headers: Vec::new(),
        body: b"pin-ok".to_vec(),
    }));
    let (pause, _) = Pause::recorded();
    let session = FetchSession::build(
        origins_for(server.addr, server.addr),
        os_trust_config().unwrap(),
        TrustRoot::pinned(),
        pause,
        false,
    )
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    assert!(run_fetch(&session, root.path()).await.is_err());
    assert_eq!(counts(&server.log, "/"), 0);

    let connector = TlsConnector::from(Arc::new(chained.client));
    let tcp = TcpStream::connect(server.addr).await.unwrap();
    let name = ServerName::IpAddress(rustls::pki_types::IpAddr::from(Ipv4Addr::LOCALHOST));
    let mut tls = connector.connect(name, tcp).await.unwrap();
    tls.write_all(b"GET /pin HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut buf = Vec::new();
    tls.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf);
    assert!(text.contains("pin-ok"));
    assert_eq!(counts(&server.log, "/pin"), 1);
}

#[tokio::test]
async fn cancellation_discards_the_partial() {
    let certs = fixture_certs();
    let publisher = publisher();
    let primary = spawn(Arc::clone(&certs.server)).await;
    let secondary = spawn(certs.server).await;
    let release = release_at(
        &publisher,
        primary.addr.port(),
        &[("udm.exe", b"hello-udm-cancel-body".as_slice())],
    );
    let started = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&started);
    let json = release.json.clone();
    let zip = release.zip.clone();
    primary.set(Arc::new(move |hit: &Hit| {
        if hit.path == "/payload.zip" {
            return Answer::Pause {
                body: zip.clone(),
                started: Arc::clone(&flag),
            };
        }
        Answer::Body {
            status: 200,
            headers: Vec::new(),
            body: json.clone(),
        }
    }));
    secondary.set(Arc::new(|_| Answer::Drop));
    let (session, _) = open_recorded(
        origins_for(primary.addr, secondary.addr),
        certs.client,
        publisher.trust,
    );
    let root = tempfile::tempdir().unwrap();
    let epoch = root.path().join("epoch.json");
    store_epoch(&epoch, 6).unwrap();
    let dest = root.path().join("out");
    let partial = dest.join(".u3-payload.partial");
    let cancel = CancellationToken::new();
    let cancel_task = cancel.clone();
    let partial_task = partial.clone();
    tokio::spawn(async move {
        for _ in 0..200 {
            if partial_task.exists() {
                if let Ok(meta) = std::fs::metadata(&partial_task) {
                    if meta.len() > 0 {
                        cancel_task.cancel();
                        return;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        cancel_task.cancel();
    });
    let err = session
        .fetch(FetchInput {
            installed_version: "0.1.5",
            now_unix: NOW,
            epoch_path: &epoch,
            dest_dir: &dest,
            cancel: &cancel,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, FetchError::Cancelled));
    assert!(started.load(Ordering::SeqCst));
    assert!(!partial.exists());
}

#[tokio::test]
async fn non_https_redirect_is_refused() {
    let certs = fixture_certs();
    let primary = spawn(Arc::clone(&certs.server)).await;
    let secondary = spawn(certs.server).await;
    let plain = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .unwrap();
    let plain_addr = plain.local_addr().unwrap();
    let accepts = Arc::new(AtomicUsize::new(0));
    let accepts_task = Arc::clone(&accepts);
    tokio::spawn(async move {
        loop {
            if let Ok((mut tcp, _)) = plain.accept().await {
                accepts_task.fetch_add(1, Ordering::SeqCst);
                let _ = tcp.shutdown().await;
            }
        }
    });
    let location = format!("http://127.0.0.1:{}/secret", plain_addr.port());
    let primary_location = location.clone();
    primary.set(Arc::new(move |_: &Hit| {
        Answer::Redirect(primary_location.clone())
    }));
    secondary.set(Arc::new(move |_: &Hit| Answer::Redirect(location.clone())));
    let (session, _) = open_recorded(
        origins_for(primary.addr, secondary.addr),
        certs.client,
        publisher().trust,
    );
    let root = tempfile::tempdir().unwrap();
    let err = run_fetch(&session, root.path()).await.unwrap_err();
    assert!(matches!(err, FetchError::OriginsExhausted(_)));
    assert_eq!(accepts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn https_redirect_to_the_asset_host_is_followed() {
    let certs = fixture_certs();
    let publisher = publisher();
    let primary = spawn(Arc::clone(&certs.server)).await;
    let secondary = spawn(certs.server).await;
    let release = release_at(
        &publisher,
        secondary.addr.port(),
        &[("udm.exe", b"hello-udm".as_slice())],
    );
    let location = format!("https://127.0.0.1:{}/manifest", secondary.addr.port());
    primary.set(Arc::new(move |_| Answer::Redirect(location.clone())));
    secondary.set(serve_release(release.json, release.zip));
    let (session, _) = open_recorded(
        origins_for(primary.addr, secondary.addr),
        certs.client,
        publisher.trust,
    );
    let root = tempfile::tempdir().unwrap();
    let got = run_fetch(&session, root.path()).await.unwrap();
    assert_eq!(got.files[0].path, "udm.exe");
    assert_eq!(counts(&primary.log, "/manifest"), 1);
    assert!(manifest_hits(&primary.log)[0].query.contains("nonce="));
    assert_eq!(counts(&secondary.log, "/manifest"), 1);
    assert_eq!(counts(&secondary.log, "/payload"), 1);
}
