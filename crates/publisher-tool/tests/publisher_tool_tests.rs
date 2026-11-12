//! Envelope rev.4 required tests for the publisher tool. Every key in
//! this file is a freshly generated TEST keypair; no fixture holds a
//! real key, and no key file is checked in.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use udm_publisher_tool::{
    keygen, load_keypair, pin_of, sha256_hex, sign_manifest, PRIVATE_KEY_FILE, PUBLIC_KEY_FILE,
};
use usagi::update_manifest::{Manifest, Refusal, TrustRoot, VerifyContext};

const NOW: i64 = 1_800_000_000;

/// A minimal stored-method zip writer (the tool's reader, like the
/// client's U3 reader, walks the central directory and ignores CRCs).
fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut central: Vec<u8> = Vec::new();
    for (name, data) in entries {
        let local_offset = out.len() as u32;
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&0u16.to_le_bytes()); // method: stored
        out.extend_from_slice(&0u16.to_le_bytes()); // time
        out.extend_from_slice(&0u16.to_le_bytes()); // date
        out.extend_from_slice(&0u32.to_le_bytes()); // crc (ignored by readers)
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra len
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);

        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes()); // version made by
        central.extend_from_slice(&20u16.to_le_bytes()); // version needed
        central.extend_from_slice(&0u16.to_le_bytes()); // flags
        central.extend_from_slice(&0u16.to_le_bytes()); // method
        central.extend_from_slice(&0u16.to_le_bytes()); // time
        central.extend_from_slice(&0u16.to_le_bytes()); // date
        central.extend_from_slice(&0u32.to_le_bytes()); // crc
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // extra
        central.extend_from_slice(&0u16.to_le_bytes()); // comment
        central.extend_from_slice(&0u16.to_le_bytes()); // disk
        central.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        central.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        central.extend_from_slice(&local_offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let cd_offset = out.len() as u32;
    let cd_size = central.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // disk
    out.extend_from_slice(&0u16.to_le_bytes()); // cd disk
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // comment len
    out
}

fn manifest_json(epoch: u64, version: &str, min_version: &str, urls: &[String]) -> String {
    let urls: Vec<String> = urls.iter().map(|u| format!("\"{u}\"")).collect();
    format!(
        "{{\"version\":\"{version}\",\"manifest_epoch\":{epoch},\"issued_at\":1,\
         \"valid_until\":2,\"files\":[],\"min_version\":\"{min_version}\",\
         \"urls\":[{}],\"signatures\":[]}}",
        urls.join(",")
    )
}

fn write(path: &Path, contents: &[u8]) {
    fs::write(path, contents).expect("write fixture");
}

/// A signing fixture: keypair dir, previous manifest at `prev_epoch`,
/// payload zip, and an input manifest whose urls carry the zip hash.
struct Fixture {
    _dir: tempfile::TempDir,
    key_path: PathBuf,
    pub_path: PathBuf,
    previous_path: PathBuf,
    payload_path: PathBuf,
    manifest_path: PathBuf,
    zip_sha: String,
}

fn fixture(prev_epoch: u64, zip_entries: &[(&str, &[u8])]) -> Fixture {
    fixture_with_urls(prev_epoch, zip_entries, None)
}

fn fixture_with_urls(
    prev_epoch: u64,
    zip_entries: &[(&str, &[u8])],
    urls_override: Option<Vec<String>>,
) -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let key_dir = root.join("keys");
    keygen(&key_dir).expect("keygen");
    let zip = make_zip(zip_entries);
    let zip_sha = sha256_hex(&zip);
    let payload_path = root.join("payload.zip");
    write(&payload_path, &zip);
    let previous_path = root.join("previous.json");
    write(
        &previous_path,
        manifest_json(prev_epoch, "0.1.0", "0.1.0", &[]).as_bytes(),
    );
    let urls = urls_override.unwrap_or_else(|| {
        vec![
            format!("https://github.com/iamcino/udm/releases/download/v0.2.0/udm.zip?sha256={zip_sha}"),
            format!("https://gitlab.com/cino-group/udm/-/releases/v0.2.0/downloads/udm.zip?sha256={zip_sha}"),
        ]
    });
    let manifest_path = root.join("manifest.json");
    write(
        &manifest_path,
        manifest_json(0, "0.2.0", "0.1.0", &urls).as_bytes(),
    );
    Fixture {
        key_path: key_dir.join(PRIVATE_KEY_FILE),
        pub_path: key_dir.join(PUBLIC_KEY_FILE),
        previous_path,
        payload_path,
        manifest_path,
        zip_sha,
        _dir: dir,
    }
}

fn public_key_of(pub_path: &Path) -> [u8; 32] {
    let text = fs::read_to_string(pub_path).expect("read pub");
    let bytes = hex::decode(text.trim()).expect("pub is hex");
    bytes.try_into().expect("pub is 32 bytes")
}

fn trust_root_of(pub_path: &Path) -> TrustRoot {
    let public_key = public_key_of(pub_path);
    TrustRoot {
        pin: pin_of(&public_key),
        public_key,
    }
}

fn default_zip() -> Vec<(&'static str, &'static [u8])> {
    vec![
        ("udm.exe", b"exe-bytes"),
        ("udm-cli.exe", b"cli-bytes"),
        ("assets/app.bin", b"asset-bytes"),
    ]
}

// Envelope test 1: keygen writes both files, prints the pin, the pin is
// the SHA-256 of the written public key; a second keygen refuses
// without touching the existing files.
#[test]
fn keygen_writes_files_prints_pin_and_refuses_overwrite() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("keys");
    let pin = keygen(&out).expect("keygen");
    let key_path = out.join(PRIVATE_KEY_FILE);
    let pub_path = out.join(PUBLIC_KEY_FILE);
    assert!(key_path.exists(), "private key file written");
    assert!(pub_path.exists(), "public key file written");
    let public_key = public_key_of(&pub_path);
    assert_eq!(pin, pin_of(&public_key), "printed pin is the key pin");
    assert_eq!(
        pin,
        sha256_hex(&public_key),
        "pin equals SHA-256 of the written public key"
    );
    let key_before = fs::read(&key_path).expect("read key");
    let pub_before = fs::read(&pub_path).expect("read pub");
    assert!(keygen(&out).is_err(), "second keygen refuses");
    assert_eq!(fs::read(&key_path).expect("read key"), key_before);
    assert_eq!(fs::read(&pub_path).expect("read pub"), pub_before);
}

// Envelope test 2: epoch is previous + 1; omitting --previous-manifest
// refuses and writes nothing; a missing or unparsable previous file
// refuses and writes nothing; none of these set epoch 1.
#[test]
fn epoch_rules_and_previous_manifest_required() {
    let fx = fixture(7, &default_zip());
    let signed = sign_manifest(
        &fx.manifest_path,
        &fx.key_path,
        Some(&fx.previous_path),
        Some(&fx.payload_path),
        NOW,
    )
    .expect("sign");
    assert_eq!(signed.manifest_epoch, 8, "epoch is previous + 1");

    let before = fs::read(&fx.manifest_path).expect("read manifest");
    assert!(
        sign_manifest(
            &fx.manifest_path,
            &fx.key_path,
            None,
            Some(&fx.payload_path),
            NOW
        )
        .is_err(),
        "omitting --previous-manifest refuses"
    );
    assert_eq!(
        fs::read(&fx.manifest_path).expect("read manifest"),
        before,
        "refusal writes nothing"
    );

    let missing = fx.manifest_path.with_file_name("no-such-previous.json");
    assert!(
        sign_manifest(
            &fx.manifest_path,
            &fx.key_path,
            Some(&missing),
            Some(&fx.payload_path),
            NOW
        )
        .is_err(),
        "missing previous file refuses"
    );
    let garbage = fx.manifest_path.with_file_name("garbage.json");
    write(&garbage, b"not a manifest");
    assert!(
        sign_manifest(
            &fx.manifest_path,
            &fx.key_path,
            Some(&garbage),
            Some(&fx.payload_path),
            NOW
        )
        .is_err(),
        "unparsable previous file refuses"
    );
    assert_eq!(
        fs::read(&fx.manifest_path).expect("read manifest"),
        before,
        "refusals write nothing and never set epoch 1"
    );
}

// Envelope test 3: valid_until is exactly issued_at + 14 * 86400,
// overwriting any input window (the fixture's input window is 1..2).
#[test]
fn validity_window_is_exactly_fourteen_days() {
    let fx = fixture(3, &default_zip());
    let signed = sign_manifest(
        &fx.manifest_path,
        &fx.key_path,
        Some(&fx.previous_path),
        Some(&fx.payload_path),
        NOW,
    )
    .expect("sign");
    assert_eq!(signed.issued_at, NOW);
    assert_eq!(signed.valid_until, NOW + 14 * 86_400);
}

// Envelope test 4: determinism with the clock, epoch, and files held
// fixed — signing the same manifest twice yields the same signature.
#[test]
fn signing_is_deterministic_with_fixed_inputs() {
    let fx = fixture(5, &default_zip());
    let original = fs::read(&fx.manifest_path).expect("read manifest");
    let first = sign_manifest(
        &fx.manifest_path,
        &fx.key_path,
        Some(&fx.previous_path),
        Some(&fx.payload_path),
        NOW,
    )
    .expect("sign once");
    write(&fx.manifest_path, &original);
    let second = sign_manifest(
        &fx.manifest_path,
        &fx.key_path,
        Some(&fx.previous_path),
        Some(&fx.payload_path),
        NOW,
    )
    .expect("sign twice");
    assert_eq!(first.signatures, second.signatures);
    assert_eq!(first.canonical_bytes(), second.canonical_bytes());
}

// Envelope test 5: a manifest signed with key A fails Manifest::verify
// under a fresh TEST trust root built from key B.
#[test]
fn manifest_signed_with_key_a_fails_under_trust_root_b() {
    let fx = fixture(2, &default_zip());
    let signed = sign_manifest(
        &fx.manifest_path,
        &fx.key_path,
        Some(&fx.previous_path),
        Some(&fx.payload_path),
        NOW,
    )
    .expect("sign");
    let other_dir = tempfile::tempdir().expect("tempdir");
    let other_keys = other_dir.path().join("keys");
    keygen(&other_keys).expect("keygen B");
    let trust_b = trust_root_of(&other_keys.join(PUBLIC_KEY_FILE));
    let ctx = VerifyContext {
        now_unix: NOW,
        last_seen_epoch: 2,
        installed_version: "0.1.0",
    };
    let refused = signed.verify(&trust_b, &ctx);
    assert!(
        matches!(
            refused,
            Err(Refusal::UnknownKey) | Err(Refusal::BadSignature)
        ),
        "expected a key refusal, got {refused:?}"
    );
}

// Envelope test 6: a payload member tampered after signing fails the
// files[] entry check (its bytes no longer hash to the signed entry).
#[test]
fn tampered_member_fails_the_signed_file_entry() {
    let entries = default_zip();
    let fx = fixture(4, &entries);
    let signed = sign_manifest(
        &fx.manifest_path,
        &fx.key_path,
        Some(&fx.previous_path),
        Some(&fx.payload_path),
        NOW,
    )
    .expect("sign");
    let tampered = make_zip(&[
        ("udm.exe", b"exe-bytes-TAMPERED"),
        ("udm-cli.exe", b"cli-bytes"),
        ("assets/app.bin", b"asset-bytes"),
    ]);
    let members = udm_publisher_tool::read_zip(&tampered).expect("read tampered zip");
    let exe = members
        .iter()
        .find(|m| m.name == "udm.exe")
        .expect("exe member");
    let entry = signed
        .files
        .iter()
        .find(|f| f.path == "udm.exe")
        .expect("exe entry");
    assert_ne!(
        sha256_hex(&exe.bytes),
        entry.sha256,
        "tampered member must not match the signed entry"
    );
    assert_ne!(
        exe.bytes.len() as u64,
        entry.size,
        "tampered member size must not match either"
    );
}

// Envelope test 7: directory entries are skipped; an unsafe member
// path refuses the signing.
#[test]
fn directory_entries_skipped_and_unsafe_paths_refused() {
    let with_dirs: Vec<(&str, &[u8])> = vec![
        ("assets/", b""),
        ("udm.exe", b"exe-bytes"),
        ("udm-cli.exe", b"cli-bytes"),
        ("assets/app.bin", b"asset-bytes"),
    ];
    let fx = fixture(1, &with_dirs);
    let signed = sign_manifest(
        &fx.manifest_path,
        &fx.key_path,
        Some(&fx.previous_path),
        Some(&fx.payload_path),
        NOW,
    )
    .expect("sign");
    assert_eq!(signed.files.len(), 3, "directory entry is not a file");
    assert!(signed.files.iter().all(|f| !f.path.ends_with('/')));

    for bad in ["../evil.exe", "/abs.exe", "assets\\evil.exe", ""] {
        let fx = fixture(1, &[(bad, b"x"), ("udm.exe", b"e")]);
        let before = fs::read(&fx.manifest_path).expect("read manifest");
        assert!(
            sign_manifest(
                &fx.manifest_path,
                &fx.key_path,
                Some(&fx.previous_path),
                Some(&fx.payload_path),
                NOW
            )
            .is_err(),
            "unsafe path {bad:?} refuses"
        );
        assert_eq!(fs::read(&fx.manifest_path).expect("read manifest"), before);
    }
}

// Envelope test 8: every urls entry of the output is HTTPS and carries
// a sha256 query equal to the whole-zip SHA-256; the parameters agree.
// A wrong parameter or a non-HTTPS entry refuses.
#[test]
fn url_rules() {
    let fx = fixture(6, &default_zip());
    let signed = sign_manifest(
        &fx.manifest_path,
        &fx.key_path,
        Some(&fx.previous_path),
        Some(&fx.payload_path),
        NOW,
    )
    .expect("sign");
    assert_eq!(signed.urls.len(), 2);
    for url in &signed.urls {
        assert!(url.starts_with("https://"), "url is HTTPS: {url}");
        assert!(
            url.contains(&format!("sha256={}", fx.zip_sha)),
            "url carries the whole-zip hash: {url}"
        );
    }

    let wrong = vec![format!(
        "https://example.com/udm.zip?sha256={}",
        "0".repeat(64)
    )];
    let fx = fixture_with_urls(6, &default_zip(), Some(wrong));
    assert!(
        sign_manifest(
            &fx.manifest_path,
            &fx.key_path,
            Some(&fx.previous_path),
            Some(&fx.payload_path),
            NOW
        )
        .is_err(),
        "a sha256 parameter that is not the archive hash refuses"
    );

    let http = vec![format!("http://example.com/udm.zip?sha256={}", fx.zip_sha)];
    let fx = fixture_with_urls(6, &default_zip(), Some(http));
    assert!(
        sign_manifest(
            &fx.manifest_path,
            &fx.key_path,
            Some(&fx.previous_path),
            Some(&fx.payload_path),
            NOW
        )
        .is_err(),
        "a non-HTTPS url refuses"
    );
}

// Envelope test 9: a second --key is refused (one key, one signature).
#[test]
fn second_key_flag_is_refused_by_the_cli() {
    let fx = fixture(2, &default_zip());
    let bin = env!("CARGO_BIN_EXE_udm-publisher-tool");
    let output = Command::new(bin)
        .args([
            "sign-manifest",
            "--manifest",
            fx.manifest_path.to_str().expect("utf8 path"),
            "--key",
            fx.key_path.to_str().expect("utf8 path"),
            "--key",
            fx.key_path.to_str().expect("utf8 path"),
            "--previous-manifest",
            fx.previous_path.to_str().expect("utf8 path"),
            "--payload",
            fx.payload_path.to_str().expect("utf8 path"),
        ])
        .output()
        .expect("run the tool");
    assert!(
        !output.status.success(),
        "a second --key must be refused: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    // And the plain CLI path with the flag omitted refuses too.
    let before = fs::read(&fx.manifest_path).expect("read manifest");
    let output = Command::new(bin)
        .args([
            "sign-manifest",
            "--manifest",
            fx.manifest_path.to_str().expect("utf8 path"),
            "--key",
            fx.key_path.to_str().expect("utf8 path"),
            "--payload",
            fx.payload_path.to_str().expect("utf8 path"),
        ])
        .output()
        .expect("run the tool");
    assert!(
        !output.status.success(),
        "omitting --previous-manifest refuses"
    );
    assert_eq!(fs::read(&fx.manifest_path).expect("read manifest"), before);
}

// Envelope test 10: the round trip — a manifest signed by the tool
// verifies under the client's Manifest::verify with a fresh TEST trust
// root from the same TEST public key, and is refused under a different
// TEST trust root (second half covered by test 5's refusal kinds; here
// the positive leg plus the on-disk reparse).
#[test]
fn round_trip_verifies_under_the_client_verifier() {
    let fx = fixture(9, &default_zip());
    sign_manifest(
        &fx.manifest_path,
        &fx.key_path,
        Some(&fx.previous_path),
        Some(&fx.payload_path),
        NOW,
    )
    .expect("sign");
    let on_disk = fs::read(&fx.manifest_path).expect("read signed manifest");
    let parsed = Manifest::from_json(&on_disk).expect("signed manifest parses");
    let trust = trust_root_of(&fx.pub_path);
    let keypair = load_keypair(&fx.key_path).expect("load keypair");
    assert_eq!(trust.pin, keypair.pin());
    let ctx = VerifyContext {
        now_unix: NOW,
        last_seen_epoch: 9,
        installed_version: "0.1.0",
    };
    let verified = parsed
        .verify(&trust, &ctx)
        .expect("client verifier accepts the tool-signed manifest");
    assert_eq!(verified.epoch(), 10);
    assert_eq!(
        verified.manifest().signatures.len(),
        1,
        "exactly one signature"
    );
    assert_eq!(verified.manifest().signatures[0].key_id, trust.pin);
}
