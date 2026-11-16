//! Offline publisher tooling for the authenticated updater (design
//! section 2.5): TEST-key generation, manifest signing, and rotation /
//! revocation record signing for the U5 trust records.
//!
//! This crate is the only piece of the updater chain that ever touches a
//! private key, and it runs offline, by hand, at release time. It has no
//! network code: it reads and writes local files only.
//!
//! HARD RULE — TEST keys only: every key generated or used here in
//! development and tests is a freshly generated TEST keypair. The real
//! publisher and recovery keys are owner-custody offline only; they are
//! never generated, stored, used, or referenced by this crate's tests or
//! fixtures. The tool itself is key-agnostic: it signs with whatever
//! Ed25519 key file it is given.
//!
//! Key file formats (one format, also documented in the CLI help):
//! `publisher.key` holds the 32-byte Ed25519 seed as 64 lowercase hex
//! characters; `publisher.pub` holds the 32-byte public key as 64
//! lowercase hex characters. The pin of a public key is the SHA-256 of
//! the public key bytes, lowercase hex (design section 2.1).

use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use flate2::read::DeflateDecoder;
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{Ed25519KeyPair, KeyPair};
use sha2::{Digest, Sha256};
use usagi::update_manifest::{
    Manifest, ManifestFile, ManifestSignature, SemVer, MAX_VALIDITY_SECS,
};
use usagi::update_records::{Ed25519Identity, RevocationRecord, RotationRecord};

/// Name of the private-key file `keygen` writes inside `--out`.
pub const PRIVATE_KEY_FILE: &str = "publisher.key";
/// Name of the public-key file `keygen` writes inside `--out`.
pub const PUBLIC_KEY_FILE: &str = "publisher.pub";

/// Why a tool operation did not happen. On any `Refuse` the tool has
/// written no output.
#[derive(Debug)]
pub enum ToolError {
    /// The operation was refused on the merits (bad input, unsafe path,
    /// epoch rule, URL rule, overwrite protection, ...).
    Refuse(String),
    /// A local file could not be read or written.
    Io { path: PathBuf, message: String },
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ToolError::Refuse(why) => write!(f, "refused: {why}"),
            ToolError::Io { path, message } => {
                write!(f, "io error at {}: {message}", path.display())
            }
        }
    }
}

impl std::error::Error for ToolError {}

fn io(path: &Path, err: std::io::Error) -> ToolError {
    ToolError::Io {
        path: path.to_path_buf(),
        message: err.to_string(),
    }
}

fn refuse(message: impl Into<String>) -> ToolError {
    ToolError::Refuse(message.into())
}

/// SHA-256 of `bytes`, lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The pin of an Ed25519 public key: SHA-256 of the raw public key
/// bytes, lowercase hex (design section 2.1). The pin is a fingerprint,
/// not the key.
pub fn pin_of(public_key: &[u8; 32]) -> String {
    sha256_hex(public_key)
}

/// A generated Ed25519 keypair, seed form.
#[derive(Debug)]
pub struct Keypair {
    seed: [u8; 32],
    /// Raw public key bytes.
    pub public_key: [u8; 32],
}

impl Keypair {
    fn ring(&self) -> Ed25519KeyPair {
        Ed25519KeyPair::from_seed_unchecked(&self.seed)
            .expect("a 32-byte seed is always a valid Ed25519 seed")
    }

    /// This keypair's pin.
    pub fn pin(&self) -> String {
        pin_of(&self.public_key)
    }
}

/// Generate one fresh keypair from the system random source.
pub fn generate_keypair() -> Result<Keypair, ToolError> {
    let rng = SystemRandom::new();
    let mut seed = [0u8; 32];
    rng.fill(&mut seed)
        .map_err(|_| refuse("system random source failed"))?;
    let ring_key = Ed25519KeyPair::from_seed_unchecked(&seed)
        .map_err(|_| refuse("generated seed was rejected"))?;
    let mut public_key = [0u8; 32];
    public_key.copy_from_slice(ring_key.public_key().as_ref());
    Ok(Keypair { seed, public_key })
}

/// Load a keypair from a private-key file (hex seed format, see the
/// crate docs). The file's contents are never printed anywhere.
pub fn load_keypair(key_path: &Path) -> Result<Keypair, ToolError> {
    let text = fs::read_to_string(key_path).map_err(|e| io(key_path, e))?;
    let bytes = hex::decode(text.trim())
        .map_err(|_| refuse(format!("key file {} is not hex", key_path.display())))?;
    let seed: [u8; 32] = bytes
        .try_into()
        .map_err(|_| refuse("key file does not hold a 32-byte Ed25519 seed"))?;
    let ring_key = Ed25519KeyPair::from_seed_unchecked(&seed)
        .map_err(|_| refuse("key file seed was rejected"))?;
    let mut public_key = [0u8; 32];
    public_key.copy_from_slice(ring_key.public_key().as_ref());
    Ok(Keypair { seed, public_key })
}

/// `keygen --out <dir>`: write a fresh TEST keypair into `dir` and
/// return its pin. Refuses, touching nothing, when either key file
/// already exists in `dir`.
pub fn keygen(out_dir: &Path) -> Result<String, ToolError> {
    let key_path = out_dir.join(PRIVATE_KEY_FILE);
    let pub_path = out_dir.join(PUBLIC_KEY_FILE);
    if key_path.exists() || pub_path.exists() {
        return Err(refuse(format!(
            "key files already exist in {}; refusing to overwrite",
            out_dir.display()
        )));
    }
    let keypair = generate_keypair()?;
    fs::create_dir_all(out_dir).map_err(|e| io(out_dir, e))?;
    write_private_key(&key_path, &hex::encode(keypair.seed))?;
    fs::write(&pub_path, format!("{}\n", hex::encode(keypair.public_key)))
        .map_err(|e| io(&pub_path, e))?;
    Ok(keypair.pin())
}

#[cfg(unix)]
fn write_private_key(path: &Path, contents: &str) -> Result<(), ToolError> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let mut file = options.open(path).map_err(|e| io(path, e))?;
    use std::io::Write;
    file.write_all(contents.as_bytes())
        .and_then(|_| file.write_all(b"\n"))
        .map_err(|e| io(path, e))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private_key(path: &Path, contents: &str) -> Result<(), ToolError> {
    fs::write(path, format!("{contents}\n")).map_err(|e| io(path, e))
}

/// `sign-manifest`: sign the manifest at `manifest_path` in place.
///
/// * `previous_manifest` is required: the new epoch is the previous
///   published manifest's epoch plus one. Omitting it, or a missing or
///   unparsable file, refuses and writes nothing. This function never
///   sets epoch 1; producing the first published manifest is the
///   operator's release process, not this tool (envelope rev.4).
/// * `payload` is required: the release `.zip`; its member set becomes
///   `files[]` and its SHA-256 must be the `sha256` query parameter of
///   every entry of the input manifest's `urls`.
/// * `issued_at` is set to `now_unix`, `valid_until` to `now_unix` plus
///   [`MAX_VALIDITY_SECS`], overwriting the input window.
/// * The output carries exactly one signature, from `key_path`'s key,
///   with `key_id` equal to that key's pin (the form the client's
///   verifier matches).
///
/// On any refusal nothing is written.
#[allow(clippy::too_many_arguments)]
pub fn sign_manifest(
    manifest_path: &Path,
    key_path: &Path,
    previous_manifest: Option<&Path>,
    payload: Option<&Path>,
    now_unix: i64,
) -> Result<Manifest, ToolError> {
    let previous_path = previous_manifest.ok_or_else(|| {
        refuse("--previous-manifest is required; this tool never sets epoch 1 by discovery")
    })?;
    let previous_bytes = fs::read(previous_path).map_err(|e| io(previous_path, e))?;
    let previous = Manifest::from_json(&previous_bytes)
        .map_err(|e| refuse(format!("previous manifest is unparsable: {e:?}")))?;
    if previous.manifest_epoch == 0 {
        return Err(refuse("previous manifest epoch is not a positive integer"));
    }
    let new_epoch = previous
        .manifest_epoch
        .checked_add(1)
        .ok_or_else(|| refuse("manifest epoch overflow"))?;

    let input_bytes = fs::read(manifest_path).map_err(|e| io(manifest_path, e))?;
    let input = Manifest::from_json(&input_bytes)
        .map_err(|e| refuse(format!("input manifest is unparsable: {e:?}")))?;
    SemVer::parse(&input.version).map_err(|e| refuse(format!("input manifest version: {e}")))?;
    SemVer::parse(&input.min_version)
        .map_err(|e| refuse(format!("input manifest min_version: {e}")))?;

    let payload_path =
        payload.ok_or_else(|| refuse("--payload is required; files[] comes from the archive"))?;
    let payload_bytes = fs::read(payload_path).map_err(|e| io(payload_path, e))?;
    let archive_sha256 = sha256_hex(&payload_bytes);
    let members = read_zip(&payload_bytes)?;
    let mut files = Vec::with_capacity(members.len());
    let mut seen = std::collections::BTreeSet::new();
    for member in &members {
        safe_relative(&member.name)?;
        if !seen.insert(member.name.clone()) {
            return Err(refuse(format!(
                "payload archive lists {} more than once",
                member.name
            )));
        }
        files.push(ManifestFile {
            path: member.name.clone(),
            sha256: sha256_hex(&member.bytes),
            size: member.bytes.len() as u64,
        });
    }

    validate_urls(&input.urls, &archive_sha256)?;

    let keypair = load_keypair(key_path)?;
    let pin = keypair.pin();

    let mut manifest = Manifest {
        version: input.version.clone(),
        manifest_epoch: new_epoch,
        issued_at: now_unix,
        valid_until: now_unix + MAX_VALIDITY_SECS,
        files,
        min_version: input.min_version.clone(),
        urls: input.urls.clone(),
        signatures: Vec::new(),
    };
    let signature = keypair.ring().sign(&manifest.canonical_bytes());
    manifest.signatures = vec![ManifestSignature {
        key_id: pin,
        signature: hex::encode(signature.as_ref()),
    }];

    let output = manifest_to_json(&manifest);
    fs::write(manifest_path, format!("{output}\n")).map_err(|e| io(manifest_path, e))?;
    Ok(manifest)
}

/// Serialize a manifest with exactly the eight fields the client's
/// closed-format parser accepts.
fn manifest_to_json(manifest: &Manifest) -> String {
    let files: Vec<serde_json::Value> = manifest
        .files
        .iter()
        .map(|f| {
            serde_json::json!({
                "path": f.path,
                "sha256": f.sha256,
                "size": f.size,
            })
        })
        .collect();
    let signatures: Vec<serde_json::Value> = manifest
        .signatures
        .iter()
        .map(|s| {
            serde_json::json!({
                "key_id": s.key_id,
                "signature": s.signature,
            })
        })
        .collect();
    let value = serde_json::json!({
        "version": manifest.version,
        "manifest_epoch": manifest.manifest_epoch,
        "issued_at": manifest.issued_at,
        "valid_until": manifest.valid_until,
        "files": files,
        "min_version": manifest.min_version,
        "urls": manifest.urls,
        "signatures": signatures,
    });
    serde_json::to_string_pretty(&value).expect("serializing plain values cannot fail")
}

/// Every URL must be HTTPS and carry a `sha256` query parameter equal
/// to the whole-archive hash; the parameters must agree (the client's
/// `archive_targets` rule, design sections 2.1 and 2.5).
fn validate_urls(urls: &[String], archive_sha256: &str) -> Result<(), ToolError> {
    if urls.is_empty() {
        return Err(refuse("manifest urls is empty; no payload URL"));
    }
    let mut expected: Option<String> = None;
    for raw in urls {
        let url = url::Url::parse(raw)
            .map_err(|_| refuse(format!("manifest url is not a valid URL: {raw}")))?;
        if url.scheme() != "https" {
            return Err(refuse(format!("manifest url is not HTTPS: {raw}")));
        }
        let mut hash = None;
        for (key, value) in url.query_pairs() {
            if key == "sha256" {
                hash = Some(value.into_owned());
            }
        }
        let hash = hash.ok_or_else(|| {
            refuse(format!(
                "manifest url lacks the sha256 query parameter: {raw}"
            ))
        })?;
        if !is_lower_hex(&hash, 64) {
            return Err(refuse(
                "manifest url sha256 parameter is not 64 lowercase hex characters",
            ));
        }
        if hash != archive_sha256 {
            return Err(refuse(
                "manifest url sha256 parameter does not equal the payload archive SHA-256",
            ));
        }
        match &expected {
            None => expected = Some(hash),
            Some(prev) if *prev != hash => {
                return Err(refuse("manifest url sha256 parameters disagree"));
            }
            _ => {}
        }
    }
    Ok(())
}

fn is_lower_hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// The client's `safe_relative` rule (design section 2.1, U3 fetch):
/// refuses an empty path, an absolute path, a backslash anywhere, or
/// any component that is not a plain name (dot, dot-dot, root, prefix).
pub fn safe_relative(path: &str) -> Result<PathBuf, ToolError> {
    if path.is_empty() || path.starts_with('/') || path.starts_with('\\') || path.contains('\\') {
        return Err(refuse(format!("unsafe path in payload archive: {path}")));
    }
    let mut out = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => out.push(part),
            _ => return Err(refuse(format!("unsafe path in payload archive: {path}"))),
        }
    }
    if out.as_os_str().is_empty() {
        return Err(refuse(format!("unsafe path in payload archive: {path}")));
    }
    Ok(out)
}

/// One payload archive member: name plus inflated contents.
#[derive(Debug)]
pub struct ZipMember {
    /// Member name as stored in the archive.
    pub name: String,
    /// Full uncompressed contents.
    pub bytes: Vec<u8>,
}

/// Read a zip archive exactly as the client's U3 fetch reader does:
/// central-directory walk, directory entries (names ending in `/`)
/// skipped, encrypted entries refused, stored and deflate methods
/// supported, anything else refused.
pub fn read_zip(data: &[u8]) -> Result<Vec<ZipMember>, ToolError> {
    let eocd = find_eocd(data)?;
    let entries = u16_at(data, eocd + 10)? as usize;
    let cd_size = u32_at(data, eocd + 12)? as usize;
    let cd_offset = u32_at(data, eocd + 16)? as usize;
    if cd_offset.saturating_add(cd_size) > data.len() {
        return Err(refuse("payload archive central directory is out of range"));
    }
    let mut cursor = cd_offset;
    let mut members = Vec::with_capacity(entries);
    for _ in 0..entries {
        if cursor + 46 > data.len() || u32_at(data, cursor)? != 0x0201_4b50 {
            return Err(refuse("payload archive has a bad central directory entry"));
        }
        let flags = u16_at(data, cursor + 8)?;
        let method = u16_at(data, cursor + 10)?;
        let comp_size = u32_at(data, cursor + 20)? as usize;
        let name_len = u16_at(data, cursor + 28)? as usize;
        let extra_len = u16_at(data, cursor + 30)? as usize;
        let comment_len = u16_at(data, cursor + 32)? as usize;
        let local_offset = u32_at(data, cursor + 42)? as usize;
        let name_start = cursor + 46;
        let name_end = name_start + name_len;
        if name_end > data.len() {
            return Err(refuse("payload archive central name is out of range"));
        }
        let name = std::str::from_utf8(&data[name_start..name_end])
            .map_err(|_| refuse("payload archive member name is not UTF-8"))?
            .to_owned();
        cursor = name_end + extra_len + comment_len;
        if name.ends_with('/') {
            continue;
        }
        if flags & 1 != 0 {
            return Err(refuse(format!(
                "payload archive member {name} is encrypted"
            )));
        }
        let bytes = read_local(data, local_offset, method, comp_size)?;
        members.push(ZipMember { name, bytes });
    }
    Ok(members)
}

fn read_local(
    data: &[u8],
    offset: usize,
    method: u16,
    comp_size: usize,
) -> Result<Vec<u8>, ToolError> {
    if offset + 30 > data.len() || u32_at(data, offset)? != 0x0403_4b50 {
        return Err(refuse("payload archive has a bad local header"));
    }
    let name_len = u16_at(data, offset + 26)? as usize;
    let extra_len = u16_at(data, offset + 28)? as usize;
    let data_start = offset + 30 + name_len + extra_len;
    let data_end = data_start + comp_size;
    if data_end > data.len() {
        return Err(refuse("payload archive local data is out of range"));
    }
    let compressed = &data[data_start..data_end];
    match method {
        0 => Ok(compressed.to_vec()),
        8 => {
            let mut decoder = DeflateDecoder::new(compressed);
            let mut out = Vec::new();
            decoder
                .read_to_end(&mut out)
                .map_err(|err| refuse(format!("payload archive deflate: {err}")))?;
            Ok(out)
        }
        other => Err(refuse(format!(
            "payload archive uses unsupported compression method {other}"
        ))),
    }
}

fn find_eocd(data: &[u8]) -> Result<usize, ToolError> {
    if data.len() < 22 {
        return Err(refuse("payload archive is too small"));
    }
    let start = data.len().saturating_sub(22 + 65535);
    let mut index = data.len() - 22;
    loop {
        if data[index..].starts_with(&0x0605_4b50u32.to_le_bytes()) {
            let comment_len = u16_at(data, index + 20)? as usize;
            if index + 22 + comment_len == data.len() {
                return Ok(index);
            }
        }
        if index == start {
            break;
        }
        index -= 1;
    }
    Err(refuse(
        "payload archive is missing its end of central directory",
    ))
}

fn u16_at(data: &[u8], index: usize) -> Result<u16, ToolError> {
    let bytes: [u8; 2] = data
        .get(index..index + 2)
        .ok_or_else(|| refuse("payload archive has a truncated field"))?
        .try_into()
        .map_err(|_| refuse("payload archive has a truncated field"))?;
    Ok(u16::from_le_bytes(bytes))
}

fn u32_at(data: &[u8], index: usize) -> Result<u32, ToolError> {
    let bytes: [u8; 4] = data
        .get(index..index + 4)
        .ok_or_else(|| refuse("payload archive has a truncated field"))?
        .try_into()
        .map_err(|_| refuse("payload archive has a truncated field"))?;
    Ok(u32::from_le_bytes(bytes))
}

// ---------------------------------------------------------------------------
// Trust records: rotation and revocation (U5 client semantics, envelope
// rev.2). The tool conforms to the landed client: it builds the record,
// signs the client's canonical bytes, and then runs the client's own
// `verify` as the refusal oracle — whatever the client verifier refuses,
// the tool refuses, and nothing is written. TEST keys only in tests; the
// compiled-pin refusals inside `verify` cannot be reached with a TEST
// seed, by design.
// ---------------------------------------------------------------------------

/// Load an Ed25519 public key from a public-key file (the `keygen`
/// format: the 32 raw bytes as 64 lowercase hex characters). A pin is
/// never read from a file anywhere in this tool; it is computed from
/// the key bytes.
pub fn load_public_key(pub_path: &Path) -> Result<[u8; 32], ToolError> {
    let text = fs::read_to_string(pub_path).map_err(|e| io(pub_path, e))?;
    let text = text.trim();
    if !is_lower_hex(text, 64) {
        return Err(refuse(format!(
            "public key file {} does not hold 64 lowercase hex characters",
            pub_path.display()
        )));
    }
    let bytes = hex::decode(text).map_err(|_| refuse("public key file is not hexadecimal"))?;
    bytes
        .try_into()
        .map_err(|_| refuse("public key file does not hold a 32-byte Ed25519 public key"))
}

/// `sign-rotation`: write one signed rotation record to `out_path`.
///
/// The signing key is the current publisher key file; the successor is
/// a public-key file (the `keygen` `.pub` format) — the tool writes
/// those key bytes as `new_public_key` and computes the successor pin
/// itself; it never accepts a declared pin in place of the key.
/// `issued_at` is `now_unix`, the signing clock supplied by the caller
/// (the CLI passes the system clock); it is not an operator input.
///
/// The record is refused, and no file is written, whenever the landed
/// client's [`RotationRecord::verify`] refuses it: a cutover less than
/// 30 days after issue (exactly 30 days is accepted), a `cutover_epoch`
/// less than `announced_epoch + 1`, `announced_epoch` equal to 0, a
/// `window_until` at or before `cutover_at` or more than 30 days after
/// it (exactly 30 days is accepted), a negative timestamp, a successor
/// equal to the signing key, a successor equal to the compiled recovery
/// key, or a signing key that is the compiled recovery key
/// (`RecoveryKeyForbidden`). Adoption-time refusals (epoch binding,
/// first-seen-after-cutover, not-newer, retiring-key-closed, revoked
/// signer or successor) stay in the client and are not decided here.
/// An existing `out_path` is never overwritten.
#[allow(clippy::too_many_arguments)]
pub fn sign_rotation(
    key_path: &Path,
    successor_pub_path: &Path,
    out_path: &Path,
    announced_epoch: u64,
    cutover_epoch: u64,
    cutover_at: i64,
    window_until: i64,
    now_unix: i64,
) -> Result<RotationRecord, ToolError> {
    if out_path.exists() {
        return Err(refuse(format!(
            "output file {} already exists; refusing to overwrite",
            out_path.display()
        )));
    }
    let signer = load_keypair(key_path)?;
    let successor_bytes = load_public_key(successor_pub_path)?;
    let signer_identity = Ed25519Identity::from_public_key(signer.public_key);
    let mut record = RotationRecord {
        successor: Ed25519Identity::from_public_key(successor_bytes),
        issued_at: now_unix,
        cutover_at,
        window_until,
        announced_epoch,
        cutover_epoch,
        signer_key_id: signer.pin(),
        signature: String::new(),
    };
    let signature = signer.ring().sign(&record.canonical_bytes());
    record.signature = hex::encode(signature.as_ref());
    record.verify(&signer_identity, now_unix).map_err(|err| {
        refuse(format!(
            "client verifier refuses this rotation record: {err}"
        ))
    })?;
    fs::write(out_path, format!("{}\n", rotation_to_json(&record))).map_err(|e| io(out_path, e))?;
    Ok(record)
}

/// `sign-revocation`: write one signed revocation record to `out_path`,
/// signed with the recovery key file, naming `revoked_key_id` (the pin
/// of the publisher key to revoke).
///
/// The record is refused, and no file is written, when `revoked_key_id`
/// is not 64 lowercase hex characters, or whenever the landed client's
/// [`RevocationRecord::verify`] refuses it: the named key is the
/// signing key itself, the named key is the compiled recovery pin
/// (`RevokesRecoveryKey`), or the signing key is the compiled publisher
/// key. `issued_at` is `now_unix`, the signing clock, not an operator
/// input. An existing `out_path` is never overwritten.
pub fn sign_revocation(
    key_path: &Path,
    revoked_key_id: &str,
    out_path: &Path,
    now_unix: i64,
) -> Result<RevocationRecord, ToolError> {
    if out_path.exists() {
        return Err(refuse(format!(
            "output file {} already exists; refusing to overwrite",
            out_path.display()
        )));
    }
    if !is_lower_hex(revoked_key_id, 64) {
        return Err(refuse("revoked key id is not 64 lowercase hex characters"));
    }
    let signer = load_keypair(key_path)?;
    let signer_identity = Ed25519Identity::from_public_key(signer.public_key);
    let mut record = RevocationRecord {
        key_id: revoked_key_id.to_owned(),
        issued_at: now_unix,
        signer_key_id: signer.pin(),
        signature: String::new(),
    };
    let signature = signer.ring().sign(&record.canonical_bytes());
    record.signature = hex::encode(signature.as_ref());
    record.verify(&signer_identity, now_unix).map_err(|err| {
        refuse(format!(
            "client verifier refuses this revocation record: {err}"
        ))
    })?;
    fs::write(out_path, format!("{}\n", revocation_to_json(&record)))
        .map_err(|e| io(out_path, e))?;
    Ok(record)
}

/// Serialize a rotation record with exactly the fields the client's
/// closed-format parser accepts.
fn rotation_to_json(record: &RotationRecord) -> String {
    let value = serde_json::json!({
        "kind": "rotation",
        "new_public_key": hex::encode(record.successor.public_key()),
        "issued_at": record.issued_at,
        "cutover_at": record.cutover_at,
        "window_until": record.window_until,
        "announced_epoch": record.announced_epoch,
        "cutover_epoch": record.cutover_epoch,
        "signatures": [{
            "key_id": record.signer_key_id,
            "signature": record.signature,
        }],
    });
    serde_json::to_string_pretty(&value).expect("serializing plain values cannot fail")
}

/// Serialize a revocation record with exactly the fields the client's
/// closed-format parser accepts.
fn revocation_to_json(record: &RevocationRecord) -> String {
    let value = serde_json::json!({
        "kind": "revocation",
        "key_id": record.key_id,
        "issued_at": record.issued_at,
        "signatures": [{
            "key_id": record.signer_key_id,
            "signature": record.signature,
        }],
    });
    serde_json::to_string_pretty(&value).expect("serializing plain values cannot fail")
}
