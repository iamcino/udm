//! U8 verification-harness fixture: a loopback TLS origin pair, test
//! publisher/recovery keys, and signed-manifest/record builders.
//!
//! The shape mirrors the in-crate loopback fixture in
//! `crates/core/src/update_fetch_tests.rs`, rebuilt here on the public
//! API only: sessions are `FetchSession::with_tls` (never `open`), the
//! payload zip carries `udm.exe`, `udm-cli.exe` and `assets/` entries,
//! and the whole-archive SHA-256 travels in the `sha256` query parameter
//! of the signed payload URL. All keys are generated fresh in-test; no
//! key material is checked in.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType,
};
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair as _};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use url::Url;

use usagi::update_fetch::{FetchInput, FetchSession, Origin};
use usagi::update_manifest::{Manifest, ManifestFile, TrustRoot};
use usagi::update_records::{Ed25519Identity, RevocationRecord, RotationRecord};

/// Signing time of the standard test release.
pub const ISSUED: i64 = 1_700_000_000;
/// Fetch time of the standard test release: one day after issue.
pub const NOW: i64 = ISSUED + 86_400;
/// Installed version every harness fetch runs against.
pub const INSTALLED: &str = "0.1.5";
/// Stored epoch seeded before a fetch of the standard release (epoch 7).
pub const STORED_EPOCH: u64 = 6;

/// Standard section 2.5 payload: both executables and an assets tree.
pub const PAYLOAD_FILES: &[(&str, &[u8])] = &[
    ("udm.exe", b"u8 new exe bytes"),
    ("udm-cli.exe", b"u8 new cli bytes"),
    ("assets/a.txt", b"u8 new asset a"),
    ("assets/b.txt", b"u8 new asset b"),
];

/// The pre-update build a case-1 install starts from.
pub const OLD_FILES: &[(&str, &[u8])] = &[
    ("udm.exe", b"u8 old exe bytes"),
    ("udm-cli.exe", b"u8 old cli bytes"),
    ("assets/a.txt", b"u8 old asset a"),
    ("assets/b.txt", b"u8 old asset b"),
];

#[derive(Clone)]
pub enum Answer {
    Drop,
    Body { status: u16, body: Vec<u8> },
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub path: String,
    pub query: String,
    pub range: Option<String>,
}

pub type Responder = Arc<dyn Fn(&Hit) -> Answer + Send + Sync>;

pub struct Loop {
    pub addr: SocketAddr,
    pub log: Arc<Mutex<Vec<Hit>>>,
    slot: Arc<Mutex<Responder>>,
    shutdown: tokio::sync::watch::Sender<bool>,
    join: tokio::task::JoinHandle<()>,
}

impl Loop {
    pub fn set(&self, responder: Responder) {
        *self.slot.lock().unwrap() = responder;
    }
}

impl Drop for Loop {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        self.join.abort();
    }
}

pub struct Certs {
    pub client: ClientConfig,
    pub server: Arc<ServerConfig>,
}

fn install_ring() {
    usagi::net::tls::install_default_provider();
}

/// A fixture CA and a 127.0.0.1 server certificate under it.
pub fn fixture_certs() -> Certs {
    install_ring();
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let mut ca_name = DistinguishedName::new();
    ca_name.push(DnType::CommonName, "UDM U8 fixture CA");
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

/// Spawn one TLS origin on an ephemeral 127.0.0.1 port.
pub async fn spawn(server: Arc<ServerConfig>) -> Loop {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let acceptor = TlsAcceptor::from(server);
    let log = Arc::new(Mutex::new(Vec::new()));
    let slot: Arc<Mutex<Responder>> = Arc::new(Mutex::new(Arc::new(|_| Answer::Body {
        status: 404,
        body: Vec::new(),
    })));
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

async fn read_hit(
    tls: &mut tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
) -> Result<Hit, ()> {
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
    tls: &mut tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
    answer: Answer,
) -> Result<(), ()> {
    match answer {
        Answer::Drop => Ok(()),
        Answer::Body { status, body } => {
            let head = format!(
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            tls.write_all(head.as_bytes()).await.map_err(|_| ())?;
            if !body.is_empty() {
                tls.write_all(&body).await.map_err(|_| ())?;
            }
            let _ = tls.shutdown().await;
            Ok(())
        }
    }
}

/// One test keypair and the trust root that pins it.
pub struct Publisher {
    pub trust: TrustRoot,
    pub key: Ed25519KeyPair,
}

pub fn publisher() -> Publisher {
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

pub fn sha256_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The manifest fields a harness case varies; defaults are the
/// standard valid release (version 0.2.0, epoch 7, 7-day window).
#[derive(Clone)]
pub struct ManifestSpec {
    pub version: String,
    pub epoch: u64,
    pub issued_at: i64,
    pub valid_until: i64,
    pub min_version: String,
    /// Payload URL hash parameter override (whole-archive mismatch).
    pub archive_hash: Option<String>,
    /// One member's manifest entry replaced by a wrong hash/size.
    pub member_override: Option<(String, String, u64)>,
}

impl Default for ManifestSpec {
    fn default() -> Self {
        Self {
            version: "0.2.0".to_owned(),
            epoch: 7,
            issued_at: ISSUED,
            valid_until: ISSUED + 7 * 86_400,
            min_version: "0.1.0".to_owned(),
            archive_hash: None,
            member_override: None,
        }
    }
}

pub struct Release {
    pub json: Vec<u8>,
    pub zip: Vec<u8>,
}

/// Build the payload zip and the manifest JSON for `spec`, signed by
/// every key in `signers` (one for a normal release, two inside the
/// dual-signature window), with the payload served from `port`.
pub fn release_at(
    signers: &[&Publisher],
    port: u16,
    files: &[(&str, &[u8])],
    spec: &ManifestSpec,
) -> Release {
    let zip = zip_archive(files);
    let archive_hash = spec
        .archive_hash
        .clone()
        .unwrap_or_else(|| sha256_bytes(&zip));
    let url = format!("https://127.0.0.1:{port}/payload.zip?sha256={archive_hash}");
    Release {
        json: signed_json(signers, &url, files, spec),
        zip,
    }
}

fn signed_json(
    signers: &[&Publisher],
    url: &str,
    files: &[(&str, &[u8])],
    spec: &ManifestSpec,
) -> Vec<u8> {
    let manifest_files: Vec<ManifestFile> = files
        .iter()
        .map(|(path, bytes)| {
            let (sha256, size) = spec
                .member_override
                .as_ref()
                .filter(|(name, _, _)| name == path)
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
        version: spec.version.clone(),
        manifest_epoch: spec.epoch,
        issued_at: spec.issued_at,
        valid_until: spec.valid_until,
        files: manifest_files,
        min_version: spec.min_version.clone(),
        urls: vec![url.to_owned()],
        signatures: Vec::new(),
    };
    let canonical = manifest.canonical_bytes();
    let entries: Vec<String> = signers
        .iter()
        .map(|signer| {
            let signature = hex::encode(signer.key.sign(&canonical).as_ref());
            format!(
                "{{\"key_id\":\"{}\",\"signature\":\"{signature}\"}}",
                signer.trust.pin
            )
        })
        .collect();
    let mut bytes = canonical;
    bytes.pop();
    bytes.extend(format!(",\"signatures\":[{}]}}", entries.join(",")).into_bytes());
    bytes
}

/// A stored-method zip archive, the same hand-rolled shape the
/// in-crate fixture writes.
pub fn zip_archive(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut locals = Vec::new();
    let mut central = Vec::new();
    let mut offset = 0u32;
    for (name, data) in files {
        let name_bytes = name.as_bytes();
        let mut local = Vec::new();
        push_u32(&mut local, 0x0403_4b50);
        push_u16(&mut local, 20);
        push_u16(&mut local, 0);
        push_u16(&mut local, 0);
        push_u16(&mut local, 0);
        push_u16(&mut local, 0);
        push_u32(&mut local, 0);
        push_u32(&mut local, data.len() as u32);
        push_u32(&mut local, data.len() as u32);
        push_u16(&mut local, name_bytes.len() as u16);
        push_u16(&mut local, 0);
        local.extend_from_slice(name_bytes);
        local.extend_from_slice(data);
        let mut entry = Vec::new();
        push_u32(&mut entry, 0x0201_4b50);
        push_u16(&mut entry, 20);
        push_u16(&mut entry, 20);
        push_u16(&mut entry, 0);
        push_u16(&mut entry, 0);
        push_u16(&mut entry, 0);
        push_u16(&mut entry, 0);
        push_u32(&mut entry, 0);
        push_u32(&mut entry, data.len() as u32);
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

/// Serve a release: the manifest JSON for every non-payload path, the
/// zip (with Range support) for `/payload.zip`.
pub fn serve_release(json: Vec<u8>, zip: Vec<u8>) -> Responder {
    Arc::new(move |hit: &Hit| body_for(hit, &json, &zip))
}

pub fn body_for(hit: &Hit, json: &[u8], zip: &[u8]) -> Answer {
    // The harness never seeds a partial file, so the client never sends
    // a Range header and the whole archive is always served with a 200.
    if hit.path == "/payload.zip" {
        return Answer::Body {
            status: 200,
            body: zip.to_vec(),
        };
    }
    Answer::Body {
        status: 200,
        body: json.to_vec(),
    }
}

pub fn counts(log: &Mutex<Vec<Hit>>, prefix: &str) -> usize {
    log.lock()
        .unwrap()
        .iter()
        .filter(|hit| hit.path.starts_with(prefix))
        .count()
}

pub fn origins_for(primary: SocketAddr, secondary: SocketAddr) -> [Origin; 2] {
    [
        Origin::new(Url::parse(&format!("https://127.0.0.1:{}/manifest", primary.port())).unwrap())
            .unwrap(),
        Origin::new(
            Url::parse(&format!("https://127.0.0.1:{}/manifest", secondary.port())).unwrap(),
        )
        .unwrap(),
    ]
}

/// A `with_tls` session over the two fixture origins. Never `open`.
pub fn session(origins: [Origin; 2], certs: &Certs, trust: TrustRoot) -> FetchSession {
    FetchSession::with_tls(origins, certs.client.clone(), trust).unwrap()
}

/// Seed the durable epoch file exactly as the fetch layer writes it.
pub fn write_epoch(path: &std::path::Path, epoch: u64) {
    std::fs::write(path, format!("{{\"last_seen_epoch\":{epoch}}}")).unwrap();
}

/// Read the durable epoch file back (0 when missing, as the layer
/// itself treats it).
pub fn read_epoch(path: &std::path::Path) -> u64 {
    match std::fs::read(path) {
        Ok(bytes) => {
            let doc: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            doc["last_seen_epoch"].as_u64().unwrap()
        }
        Err(_) => 0,
    }
}

/// One harness fetch at `now_unix` against a fresh destination, with
/// the epoch file at `<root>/epoch.json` and output at `<root>/out`.
pub async fn run_fetch_at(
    session: &FetchSession,
    root: &std::path::Path,
    now_unix: i64,
) -> Result<usagi::update_fetch::VerifiedPayload, usagi::update_fetch::FetchError> {
    let epoch = root.join("epoch.json");
    session
        .fetch(FetchInput {
            installed_version: INSTALLED,
            now_unix,
            epoch_path: &epoch,
            dest_dir: &root.join("out"),
            cancel: &CancellationToken::new(),
        })
        .await
}

/// Write a file tree of `(relative path, bytes)` under `base`.
pub fn write_tree(base: &std::path::Path, files: &[(&str, &[u8])]) {
    for (rel, bytes) in files {
        let mut path = base.to_path_buf();
        for part in rel.split('/') {
            path.push(part);
        }
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
    }
}

/// A revocation record naming `revoked`'s pin, signed by `recovery`,
/// with the signature computed over the record's own canonical bytes.
pub fn revocation_record_bytes(
    revoked: &Publisher,
    recovery: &Publisher,
    issued_at: i64,
) -> Vec<u8> {
    let record = RevocationRecord {
        key_id: revoked.trust.pin.clone(),
        issued_at,
        signer_key_id: recovery.trust.pin.clone(),
        signature: String::new(),
    };
    let signature = hex::encode(recovery.key.sign(&record.canonical_bytes()).as_ref());
    format!(
        "{{\"issued_at\":{issued_at},\"key_id\":\"{}\",\"kind\":\"revocation\",\"signatures\":[{{\"key_id\":\"{}\",\"signature\":\"{signature}\"}}]}}",
        revoked.trust.pin, recovery.trust.pin
    )
    .into_bytes()
}

/// A rotation record announcing `successor`, signed by the current
/// publisher `current`, serialized in canonical field order.
pub fn rotation_record_bytes(
    current: &Publisher,
    successor: &Publisher,
    issued_at: i64,
    cutover_at: i64,
    window_until: i64,
    announced_epoch: u64,
    cutover_epoch: u64,
) -> Vec<u8> {
    let record = RotationRecord {
        successor: Ed25519Identity::from_public_key(successor.trust.public_key),
        issued_at,
        cutover_at,
        window_until,
        announced_epoch,
        cutover_epoch,
        signer_key_id: current.trust.pin.clone(),
        signature: String::new(),
    };
    let signature = hex::encode(current.key.sign(&record.canonical_bytes()).as_ref());
    format!(
        "{{\"announced_epoch\":{announced_epoch},\"cutover_at\":{cutover_at},\"cutover_epoch\":{cutover_epoch},\"issued_at\":{issued_at},\"kind\":\"rotation\",\"new_public_key\":\"{}\",\"window_until\":{window_until},\"signatures\":[{{\"key_id\":\"{}\",\"signature\":\"{signature}\"}}]}}",
        hex::encode(successor.trust.public_key),
        current.trust.pin
    )
    .into_bytes()
}
