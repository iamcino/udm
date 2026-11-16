//! Envelope rev.2 tests for the record-signing commands. Every key in
//! this file is a freshly generated TEST keypair; no fixture holds a
//! real key, and no key file is checked in. The compiled recovery /
//! publisher identities are referenced only by their public constants
//! (public keys and pins) to prove the tool refuses them; no real seed
//! exists here to sign with them, by design.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use udm_publisher_tool::{
    keygen, pin_of, sha256_hex, sign_manifest, sign_revocation, sign_rotation, ToolError,
    PRIVATE_KEY_FILE, PUBLIC_KEY_FILE,
};
use usagi::update_fetch::{FetchError, FetchSession, Origin};
use usagi::update_manifest::{
    Manifest, ManifestSignature, Refusal, TrustRoot, VerifiedManifest, VerifyContext,
};
use usagi::update_records::{Ed25519Identity, RevocationRecord, RotationRecord};

const T0: i64 = 1_800_000_000;
const DAY: i64 = 24 * 60 * 60;

/// Three TEST keypairs: A the current publisher, B its successor, C
/// the recovery-role key.
struct Keys {
    _dir: tempfile::TempDir,
    a_key: PathBuf,
    a_pub: PathBuf,
    b_key: PathBuf,
    b_pub: PathBuf,
    c_key: PathBuf,
    c_pub: PathBuf,
}

fn keys() -> Keys {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let (mut a_key, mut a_pub) = (PathBuf::new(), PathBuf::new());
    let (mut b_key, mut b_pub) = (PathBuf::new(), PathBuf::new());
    let (mut c_key, mut c_pub) = (PathBuf::new(), PathBuf::new());
    for (name, key_slot, pub_slot) in [
        ("a", &mut a_key, &mut a_pub),
        ("b", &mut b_key, &mut b_pub),
        ("c", &mut c_key, &mut c_pub),
    ] {
        let key_dir = root.join(name);
        keygen(&key_dir).expect("keygen");
        *key_slot = key_dir.join(PRIVATE_KEY_FILE);
        *pub_slot = key_dir.join(PUBLIC_KEY_FILE);
    }
    Keys {
        _dir: dir,
        a_key,
        a_pub,
        b_key,
        b_pub,
        c_key,
        c_pub,
    }
}

fn pub_bytes(pub_path: &Path) -> [u8; 32] {
    let text = fs::read_to_string(pub_path).expect("read pub");
    let bytes = hex::decode(text.trim()).expect("pub is hex");
    bytes.try_into().expect("pub is 32 bytes")
}

fn pin_of_pub(pub_path: &Path) -> String {
    pin_of(&pub_bytes(pub_path))
}

fn trust_root_of(pub_path: &Path) -> TrustRoot {
    let public_key = pub_bytes(pub_path);
    TrustRoot {
        pin: pin_of(&public_key),
        public_key,
    }
}

fn identity_of(pub_path: &Path) -> Ed25519Identity {
    Ed25519Identity::from_public_key(pub_bytes(pub_path))
}

/// A client session trusting key A, with key C installed as the
/// recovery identity. The origins are never contacted: the accept_*
/// methods and `client_trust` are purely local.
fn session(keys: &Keys) -> FetchSession {
    let tls = usagi::net::tls::client_config(&[b"http/1.1"]).expect("client config");
    let origin = || {
        Origin::new(url::Url::parse("https://releases.example.invalid/manifest.json").unwrap())
            .expect("origin")
    };
    let session = FetchSession::with_tls([origin(), origin()], tls, trust_root_of(&keys.a_pub))
        .expect("session");
    session
        .set_recovery_key(pub_bytes(&keys.c_pub), &pin_of_pub(&keys.c_pub))
        .expect("set recovery key");
    session
}

fn verify(
    session: &FetchSession,
    manifest: &Manifest,
    now: i64,
    last_seen_epoch: u64,
) -> Result<VerifiedManifest, Refusal> {
    let decision = session
        .client_trust()
        .decision(now, manifest.manifest_epoch);
    manifest.verify_governed(
        &decision,
        &[],
        &VerifyContext {
            now_unix: now,
            last_seen_epoch,
            installed_version: "0.1.0",
        },
    )
}

fn refused<T>(result: Result<T, ToolError>) -> String {
    match result {
        Err(ToolError::Refuse(message)) => message,
        Err(other) => panic!("expected a refusal, got io error: {other}"),
        Ok(_) => panic!("expected a refusal, got success"),
    }
}

// ---------------------------------------------------------------------------
// Rotation: happy path and wire shape
// ---------------------------------------------------------------------------

#[test]
fn rotation_happy_path_writes_a_record_the_client_verifier_accepts() {
    let keys = keys();
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("rotation.json");
    let cutover_at = T0 + 31 * DAY;
    let record = sign_rotation(
        &keys.a_key,
        &keys.b_pub,
        &out,
        5,
        7,
        cutover_at,
        cutover_at + 10 * DAY,
        T0,
    )
    .expect("sign rotation");
    assert_eq!(record.successor.pin(), pin_of_pub(&keys.b_pub));
    assert_eq!(record.signer_key_id, pin_of_pub(&keys.a_pub));
    assert_eq!(record.issued_at, T0);

    let bytes = fs::read(&out).expect("record written");
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("record is json");
    assert_eq!(value["kind"], "rotation");
    assert_eq!(
        value["new_public_key"],
        hex::encode(pub_bytes(&keys.b_pub)),
        "the successor key bytes are written, never a declared pin"
    );
    assert_eq!(
        value["signatures"].as_array().expect("signatures").len(),
        1,
        "exactly one signature"
    );
    let parsed = RotationRecord::from_json(&bytes).expect("client parses the record");
    assert_eq!(parsed, record);
    parsed
        .verify(&identity_of(&keys.a_pub), T0)
        .expect("client verifier accepts the tool's bytes");
}

#[test]
fn rotation_boundaries_exactly_30_day_lead_and_window_are_accepted() {
    let keys = keys();
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("rotation.json");
    let cutover_at = T0 + 30 * DAY;
    sign_rotation(
        &keys.a_key,
        &keys.b_pub,
        &out,
        3,
        4, // exactly announced_epoch + 1
        cutover_at,
        cutover_at + 30 * DAY,
        T0,
    )
    .expect("boundary values are accepted");
    assert!(out.exists());
}

// ---------------------------------------------------------------------------
// Rotation: the refusal suite (each refuses and writes no file)
// ---------------------------------------------------------------------------

fn rotation_case(
    keys: &Keys,
    announced_epoch: u64,
    cutover_epoch: u64,
    cutover_at: i64,
    window_until: i64,
) -> (String, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("rotation.json");
    let result = sign_rotation(
        &keys.a_key,
        &keys.b_pub,
        &out,
        announced_epoch,
        cutover_epoch,
        cutover_at,
        window_until,
        T0,
    );
    let message = refused(result);
    assert!(!out.exists(), "a refused rotation writes no file");
    (message, out)
}

#[test]
fn rotation_refused_when_lead_is_shorter_than_30_days() {
    let keys = keys();
    let cutover_at = T0 + 29 * DAY;
    let (message, _) = rotation_case(&keys, 5, 7, cutover_at, cutover_at + 10 * DAY);
    assert!(message.contains("less than 30 days"), "{message}");
}

#[test]
fn rotation_refused_when_cutover_epoch_is_not_past_announced() {
    let keys = keys();
    let cutover_at = T0 + 31 * DAY;
    let (message, _) = rotation_case(&keys, 5, 5, cutover_at, cutover_at + 10 * DAY);
    assert!(
        message.contains("not at least one release past"),
        "{message}"
    );
}

#[test]
fn rotation_refused_when_announced_epoch_is_zero() {
    let keys = keys();
    let cutover_at = T0 + 31 * DAY;
    let (message, _) = rotation_case(&keys, 0, 7, cutover_at, cutover_at + 10 * DAY);
    assert!(message.contains("announced_epoch starts at 1"), "{message}");
}

#[test]
fn rotation_refused_when_dual_window_is_empty() {
    let keys = keys();
    let cutover_at = T0 + 31 * DAY;
    let (message, _) = rotation_case(&keys, 5, 7, cutover_at, cutover_at);
    assert!(message.contains("dual-signature window"), "{message}");
}

#[test]
fn rotation_refused_when_dual_window_is_longer_than_30_days() {
    let keys = keys();
    let cutover_at = T0 + 31 * DAY;
    let (message, _) = rotation_case(&keys, 5, 7, cutover_at, cutover_at + 30 * DAY + 1);
    assert!(message.contains("dual-signature window"), "{message}");
}

#[test]
fn rotation_refused_when_a_timestamp_is_negative() {
    let keys = keys();
    let (message, _) = rotation_case(&keys, 5, 7, -1, 10 * DAY);
    assert!(message.contains("negative timestamp"), "{message}");
}

#[test]
fn rotation_refused_when_successor_is_the_signing_key() {
    let keys = keys();
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("rotation.json");
    let cutover_at = T0 + 31 * DAY;
    let result = sign_rotation(
        &keys.a_key,
        &keys.a_pub, // successor == signer
        &out,
        5,
        7,
        cutover_at,
        cutover_at + 10 * DAY,
        T0,
    );
    let message = refused(result);
    assert!(
        message.contains("successor is the signing key"),
        "{message}"
    );
    assert!(!out.exists());
}

#[test]
fn rotation_refused_when_successor_is_the_compiled_recovery_key() {
    let keys = keys();
    let dir = tempfile::tempdir().expect("tempdir");
    // The compiled recovery PUBLIC key (a public constant) offered as
    // the successor file. No recovery seed exists in this test.
    let successor = dir.path().join("successor.pub");
    fs::write(
        &successor,
        format!("{}\n", hex::encode(usagi::update_pin::RECOVERY_PUBLIC_KEY)),
    )
    .expect("write successor");
    let out = dir.path().join("rotation.json");
    let cutover_at = T0 + 31 * DAY;
    let result = sign_rotation(
        &keys.a_key,
        &successor,
        &out,
        5,
        7,
        cutover_at,
        cutover_at + 10 * DAY,
        T0,
    );
    let message = refused(result);
    assert!(
        message.contains("successor is the recovery key"),
        "{message}"
    );
    assert!(!out.exists());
}

#[test]
fn rotation_refused_when_successor_file_is_not_a_public_key() {
    let keys = keys();
    let dir = tempfile::tempdir().expect("tempdir");
    let successor = dir.path().join("successor.pub");
    fs::write(&successor, "not-a-key\n").expect("write successor");
    let out = dir.path().join("rotation.json");
    let cutover_at = T0 + 31 * DAY;
    let result = sign_rotation(
        &keys.a_key,
        &successor,
        &out,
        5,
        7,
        cutover_at,
        cutover_at + 10 * DAY,
        T0,
    );
    let message = refused(result);
    assert!(message.contains("64 lowercase hex"), "{message}");
    assert!(!out.exists());
}

#[test]
fn rotation_refused_when_output_already_exists() {
    let keys = keys();
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("rotation.json");
    fs::write(&out, b"keep me").expect("seed output");
    let cutover_at = T0 + 31 * DAY;
    let result = sign_rotation(
        &keys.a_key,
        &keys.b_pub,
        &out,
        5,
        7,
        cutover_at,
        cutover_at + 10 * DAY,
        T0,
    );
    let message = refused(result);
    assert!(message.contains("refusing to overwrite"), "{message}");
    assert_eq!(fs::read(&out).expect("read back"), b"keep me");
}

// ---------------------------------------------------------------------------
// Revocation: happy path and the refusal suite
// ---------------------------------------------------------------------------

#[test]
fn revocation_happy_path_writes_a_record_the_client_verifier_accepts() {
    let keys = keys();
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("revocation.json");
    let record =
        sign_revocation(&keys.c_key, &pin_of_pub(&keys.a_pub), &out, T0).expect("sign revocation");
    assert_eq!(record.key_id, pin_of_pub(&keys.a_pub));
    assert_eq!(record.signer_key_id, pin_of_pub(&keys.c_pub));

    let bytes = fs::read(&out).expect("record written");
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("record is json");
    assert_eq!(value["kind"], "revocation");
    assert_eq!(
        value["signatures"].as_array().expect("signatures").len(),
        1,
        "exactly one signature"
    );
    let parsed = RevocationRecord::from_json(&bytes).expect("client parses the record");
    assert_eq!(parsed, record);
    parsed
        .verify(&identity_of(&keys.c_pub), T0)
        .expect("client verifier accepts the tool's bytes");
}

fn revocation_case(keys: &Keys, key_id: &str) -> (String, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("revocation.json");
    let result = sign_revocation(&keys.c_key, key_id, &out, T0);
    let message = refused(result);
    assert!(!out.exists(), "a refused revocation writes no file");
    (message, out)
}

#[test]
fn revocation_refused_when_key_id_is_not_lowercase_hex() {
    let keys = keys();
    for bad in ["A".repeat(64), "z".repeat(64), "abcd".to_owned()] {
        let (message, _) = revocation_case(&keys, &bad);
        assert!(message.contains("64 lowercase hex"), "{message}");
    }
}

#[test]
fn revocation_refused_when_key_id_is_the_signing_key() {
    let keys = keys();
    let (message, _) = revocation_case(&keys, &pin_of_pub(&keys.c_pub));
    assert!(
        message.contains("cannot name the recovery key"),
        "{message}"
    );
}

#[test]
fn revocation_refused_when_key_id_is_the_compiled_recovery_pin() {
    let keys = keys();
    // The compiled recovery PIN (a public constant) as the target,
    // signed by a TEST recovery key: still refused.
    let (message, _) = revocation_case(&keys, usagi::update_pin::RECOVERY_PIN_SHA256);
    assert!(
        message.contains("cannot name the recovery key"),
        "{message}"
    );
}

// ---------------------------------------------------------------------------
// Round trips through the landed client (the binding acceptance proof)
// ---------------------------------------------------------------------------

/// A minimal stored-method zip writer (same shape as the U6 tests).
fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut central: Vec<u8> = Vec::new();
    for (name, data) in entries {
        let local_offset = out.len() as u32;
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&local_offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let cd_offset = out.len() as u32;
    let cd_size = central.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

fn manifest_json(epoch: u64, urls: &[String]) -> String {
    let urls: Vec<String> = urls.iter().map(|u| format!("\"{u}\"")).collect();
    format!(
        "{{\"version\":\"0.2.0\",\"manifest_epoch\":{epoch},\"issued_at\":1,\
         \"valid_until\":2,\"files\":[],\"min_version\":\"0.1.0\",\
         \"urls\":[{}],\"signatures\":[]}}",
        urls.join(",")
    )
}

/// Sign one manifest with one key via the tool, from a previous
/// manifest at `prev_epoch`; returns the parsed signed manifest.
fn tool_sign_manifest(
    root: &Path,
    name: &str,
    key_path: &Path,
    prev_epoch: u64,
    now: i64,
) -> Manifest {
    let zip = make_zip(&[("udm.exe", b"exe-bytes"), ("udm-cli.exe", b"cli-bytes")]);
    let zip_sha = sha256_hex(&zip);
    let payload_path = root.join(format!("{name}-payload.zip"));
    fs::write(&payload_path, &zip).expect("write payload");
    let previous_path = root.join(format!("{name}-previous.json"));
    fs::write(&previous_path, manifest_json(prev_epoch, &[])).expect("write previous");
    let urls = vec![format!(
        "https://github.com/iamcino/udm/releases/download/v0.2.0/udm.zip?sha256={zip_sha}"
    )];
    let manifest_path = root.join(format!("{name}-manifest.json"));
    fs::write(&manifest_path, manifest_json(0, &urls)).expect("write manifest");
    sign_manifest(
        &manifest_path,
        key_path,
        Some(&previous_path),
        Some(&payload_path),
        now,
    )
    .expect("sign manifest")
}

#[test]
fn round_trip_rotation_then_dual_signed_manifest_verifies_with_both_pins() {
    let keys = keys();
    let session = session(&keys);
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();

    // The tool produces the rotation record: A announces B.
    let cutover_at = T0 + 31 * DAY;
    let out = root.join("rotation.json");
    sign_rotation(
        &keys.a_key,
        &keys.b_pub,
        &out,
        5,
        7,
        cutover_at,
        cutover_at + 10 * DAY,
        T0,
    )
    .expect("sign rotation");
    let bytes = fs::read(&out).expect("record bytes");

    // The client adopts it before cutover.
    session
        .accept_rotation_record(&bytes, T0 + DAY, 5)
        .expect("client accepts the tool's rotation record");

    // A dual-signed manifest at the cutover epoch: two sign-manifest
    // calls, one per key over identical inputs, signatures combined.
    let signed_at = cutover_at;
    let by_a = tool_sign_manifest(&root, "a", &keys.a_key, 6, signed_at);
    let by_b = tool_sign_manifest(&root, "b", &keys.b_key, 6, signed_at);
    assert_eq!(by_a.manifest_epoch, 7);
    assert_eq!(by_a.canonical_bytes(), by_b.canonical_bytes());
    let mut combined = by_a.clone();
    combined.signatures = vec![
        ManifestSignature {
            key_id: by_a.signatures[0].key_id.clone(),
            signature: by_a.signatures[0].signature.clone(),
        },
        ManifestSignature {
            key_id: by_b.signatures[0].key_id.clone(),
            signature: by_b.signatures[0].signature.clone(),
        },
    ];

    // Inside the dual window both keys authorize.
    let verified = verify(&session, &combined, cutover_at + DAY, 6)
        .expect("dual-signed manifest verifies inside the window");
    let mut pins = verified.signer_pins().to_vec();
    pins.sort();
    let mut expected = vec![pin_of_pub(&keys.a_pub), pin_of_pub(&keys.b_pub)];
    expected.sort();
    assert_eq!(pins, expected, "the retiring key and the successor");
}

#[test]
fn round_trip_revocation_then_the_client_refuses_that_publisher() {
    let keys = keys();
    let session = session(&keys);
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();

    // A manifest signed by A alone verifies before the revocation.
    let by_a = tool_sign_manifest(&root, "a", &keys.a_key, 5, T0);
    verify(&session, &by_a, T0, 5).expect("manifest verifies before revocation");

    // The tool produces the revocation, signed by the recovery key C.
    let out = root.join("revocation.json");
    sign_revocation(&keys.c_key, &pin_of_pub(&keys.a_pub), &out, T0).expect("sign revocation");
    let bytes = fs::read(&out).expect("record bytes");
    session
        .accept_revocation_record(&bytes, T0)
        .expect("client accepts the tool's revocation record");

    // The same manifest is now refused: its only signer is revoked.
    let refused = verify(&session, &by_a, T0, 5).expect_err("revoked publisher refuses");
    assert!(
        matches!(refused, Refusal::RevokedKey { key_id } if key_id == pin_of_pub(&keys.a_pub)),
        "revoked publisher is refused as RevokedKey"
    );
}

// ---------------------------------------------------------------------------
// Key separation at the client (TEST-key cross-wiring)
// ---------------------------------------------------------------------------

#[test]
fn cross_wired_rotation_signed_by_the_recovery_key_fails_client_trust() {
    let keys = keys();
    let session = session(&keys);
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("rotation.json");
    let cutover_at = T0 + 31 * DAY;
    // The tool is key-agnostic: C (not the compiled recovery key) can
    // sign a well-formed rotation. The client must refuse it: a
    // rotation is the publisher's to sign.
    sign_rotation(
        &keys.c_key,
        &keys.b_pub,
        &out,
        5,
        7,
        cutover_at,
        cutover_at + 10 * DAY,
        T0,
    )
    .expect("tool signs with the key it is given");
    let bytes = fs::read(&out).expect("record bytes");
    let err = session
        .accept_rotation_record(&bytes, T0 + DAY, 5)
        .expect_err("client refuses a recovery-signed rotation");
    assert!(matches!(err, FetchError::Trust(_)), "{err:?}");
}

#[test]
fn cross_wired_revocation_signed_by_the_publisher_key_fails_client_trust() {
    let keys = keys();
    let session = session(&keys);
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("revocation.json");
    // A revocation signed by publisher A instead of recovery C.
    sign_revocation(&keys.a_key, &pin_of_pub(&keys.b_pub), &out, T0)
        .expect("tool signs with the key it is given");
    let bytes = fs::read(&out).expect("record bytes");
    let err = session
        .accept_revocation_record(&bytes, T0)
        .expect_err("client refuses a publisher-signed revocation");
    assert!(matches!(err, FetchError::Trust(_)), "{err:?}");
}

// ---------------------------------------------------------------------------
// CLI end-to-end (TEST keys, the real binary)
// ---------------------------------------------------------------------------

fn cli(args: &[String]) -> std::process::Output {
    let bin = env!("CARGO_BIN_EXE_udm-publisher-tool");
    Command::new(bin)
        .args(args)
        .output()
        .expect("run the tool binary")
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64
}

#[test]
fn cli_sign_rotation_and_revocation_end_to_end() {
    let keys = keys();
    let dir = tempfile::tempdir().expect("tempdir");
    let now = now_secs();
    let cutover_at = now + 31 * DAY;
    let rotation_out = dir.path().join("rotation.json");
    let output = cli(&[
        "sign-rotation".into(),
        "--key".into(),
        keys.a_key.to_string_lossy().into_owned(),
        "--successor-pub".into(),
        keys.b_pub.to_string_lossy().into_owned(),
        "--out".into(),
        rotation_out.to_string_lossy().into_owned(),
        "--announced-epoch".into(),
        "5".into(),
        "--cutover-epoch".into(),
        "7".into(),
        "--cutover-at".into(),
        cutover_at.to_string(),
        "--window-until".into(),
        (cutover_at + 10 * DAY).to_string(),
    ]);
    assert!(
        output.status.success(),
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(stdout.contains(&pin_of_pub(&keys.b_pub)), "{stdout}");
    let parsed =
        RotationRecord::from_json(&fs::read(&rotation_out).expect("record")).expect("parses");
    parsed
        .verify(&identity_of(&keys.a_pub), now)
        .expect("CLI record verifies");

    let revocation_out = dir.path().join("revocation.json");
    let output = cli(&[
        "sign-revocation".into(),
        "--key".into(),
        keys.c_key.to_string_lossy().into_owned(),
        "--key-id".into(),
        pin_of_pub(&keys.a_pub),
        "--out".into(),
        revocation_out.to_string_lossy().into_owned(),
    ]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let parsed =
        RevocationRecord::from_json(&fs::read(&revocation_out).expect("record")).expect("parses");
    assert_eq!(parsed.key_id, pin_of_pub(&keys.a_pub));
}

#[test]
fn cli_sign_rotation_with_a_short_lead_fails_and_writes_nothing() {
    let keys = keys();
    let dir = tempfile::tempdir().expect("tempdir");
    let now = now_secs();
    let out = dir.path().join("rotation.json");
    let output = cli(&[
        "sign-rotation".into(),
        "--key".into(),
        keys.a_key.to_string_lossy().into_owned(),
        "--successor-pub".into(),
        keys.b_pub.to_string_lossy().into_owned(),
        "--out".into(),
        out.to_string_lossy().into_owned(),
        "--announced-epoch".into(),
        "5".into(),
        "--cutover-epoch".into(),
        "7".into(),
        "--cutover-at".into(),
        (now + DAY).to_string(),
        "--window-until".into(),
        (now + 11 * DAY).to_string(),
    ]);
    assert!(!output.status.success(), "a short lead must fail");
    assert!(
        !String::from_utf8_lossy(&output.stderr).is_empty(),
        "the refusal is explained on stderr"
    );
    assert!(!out.exists(), "no partial output file on failure");
}
