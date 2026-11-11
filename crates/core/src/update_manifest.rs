//! Update manifest data model and Ed25519 signature verification.
//!
//! Unit U2 of the updater implementation, against the merged U2 design
//! (REV1, section 2.1). This module is the *decision* layer only: it parses
//! a signed manifest, reproduces its canonical bytes, verifies the
//! publisher signature against the pinned trust root (see
//! [`crate::update_pin`]), and enforces version monotonicity, epoch
//! monotonicity, and the freshness window. It performs no download, no
//! staging, and no prompting: every failure is a typed [`Refusal`], and the
//! caller (fetch/transport, unit U3) must treat any refusal as "no
//! download, no staging, no prompt, local log entry only", exactly as the
//! design specifies.
//!
//! Canonical form (design section 2.1): JSON with sorted keys and no
//! whitespace, over the manifest *without* its `signatures` array. The
//! canonical bytes are built field by field in sorted order so the result
//! does not depend on any JSON library's map ordering.
//!
//! Timestamp fields are Unix seconds (UTC). `key_id` in a signature entry
//! is the signer's pin: the lowercase hex SHA-256 fingerprint of its Ed25519
//! public key, the same identifier the pin module publishes.

use ring::signature;
use serde::Deserialize;

use crate::update_pin;

/// Maximum span between `issued_at` and `valid_until`: 14 days (design
/// section 2.1, freshness window).
pub const MAX_VALIDITY_SECS: i64 = 14 * 86_400;

/// Maximum amount `issued_at` may be ahead of local time: 1 day (design
/// section 2.1, clock-skew bound).
pub const MAX_ISSUED_AHEAD_SECS: i64 = 86_400;

/// One payload file named by a manifest.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestFile {
    /// Payload path, as published.
    pub path: String,
    /// SHA-256 of the file contents, lowercase hex (64 characters).
    pub sha256: String,
    /// File size in bytes.
    pub size: u64,
}

/// One Ed25519 signature over the manifest's canonical bytes.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestSignature {
    /// Signer identifier: the pin (SHA-256 fingerprint, lowercase hex) of
    /// the signing public key.
    pub key_id: String,
    /// Ed25519 signature, lowercase hex (128 characters, 64 bytes).
    pub signature: String,
}

/// A signed update manifest (design section 2.1, fields normative).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Semantic version of the release.
    pub version: String,
    /// Monotonically increasing epoch, starting at 1 for the first
    /// published manifest, +1 for every subsequent manifest, independent
    /// of the semantic version.
    pub manifest_epoch: u64,
    /// UTC signing time, Unix seconds.
    pub issued_at: i64,
    /// End of the acceptance window, Unix seconds; at most `issued_at`
    /// plus 14 days.
    pub valid_until: i64,
    /// Payload files covered by this manifest.
    pub files: Vec<ManifestFile>,
    /// Oldest installed version allowed to apply this update.
    pub min_version: String,
    /// Download URLs for the payload archive.
    pub urls: Vec<String>,
    /// Signatures over the canonical manifest bytes; two entries during a
    /// key rotation window, otherwise one.
    pub signatures: Vec<ManifestSignature>,
}

/// The trust root a manifest is verified against: an Ed25519 public key
/// and its pin. Production code uses [`TrustRoot::pinned`]; tests inject
/// their own keypair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustRoot {
    /// Ed25519 public key, raw bytes.
    pub public_key: [u8; 32],
    /// Pin: SHA-256 fingerprint of `public_key`, lowercase hex.
    pub pin: String,
}

impl TrustRoot {
    /// The compiled-in publisher trust root from the pin module (the
    /// update trust root of the design).
    pub fn pinned() -> Self {
        Self {
            public_key: update_pin::publisher_public_key(),
            pin: update_pin::publisher_pin().to_owned(),
        }
    }
}

/// Inputs the verification decision depends on, supplied by the caller so
/// this module stays pure and testable (no clock, no disk, no network).
#[derive(Debug, Clone, Copy)]
pub struct VerifyContext<'a> {
    /// Local time, Unix seconds.
    pub now_unix: i64,
    /// The durably stored epoch of the last accepted manifest (0 when no
    /// manifest has ever been accepted).
    pub last_seen_epoch: u64,
    /// The currently installed version, semantic version string.
    pub installed_version: &'a str,
}

/// A manifest that passed every verification check. The only way to
/// obtain one is [`Manifest::verify`]; downstream units (download,
/// staging) should accept this type, not a bare [`Manifest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedManifest {
    manifest: Manifest,
}

impl VerifiedManifest {
    /// The verified manifest.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// The epoch the caller must durably store as the new
    /// `last_seen_epoch` before any payload download (design section 2.1).
    pub fn epoch(&self) -> u64 {
        self.manifest.manifest_epoch
    }

    /// Consume the wrapper and return the manifest.
    pub fn into_manifest(self) -> Manifest {
        self.manifest
    }
}

/// Every way manifest verification can refuse a manifest. Any refusal
/// means: no download, no staging, no prompt (design section 2.1).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// The compiled pin constants fail their self-consistency check;
    /// nothing may be trusted until the build is replaced.
    #[error("update pins are not self-consistent; refusing all manifests")]
    PinsInconsistent,
    /// The manifest bytes or fields are not well-formed.
    #[error("manifest is malformed: {0}")]
    Malformed(String),
    /// The manifest carries no signatures at all.
    #[error("manifest carries no signatures")]
    Unsigned,
    /// No signature entry names the pinned publisher key (for example a
    /// manifest signed only by an unknown or rotated-away key).
    #[error("manifest carries no signature for the pinned publisher key")]
    UnknownKey,
    /// A signature entry names the pinned key but does not verify (or is
    /// not a decodable 64-byte Ed25519 signature).
    #[error("manifest signature does not verify for the pinned publisher key")]
    BadSignature,
    /// The epoch does not strictly increase over the stored epoch: a
    /// replay, even if the signature is valid and the version is newer.
    #[error(
        "manifest epoch {manifest_epoch} does not exceed stored epoch {last_seen_epoch}; refusing as replay"
    )]
    EpochNotIncreasing {
        /// The epoch carried by the refused manifest.
        manifest_epoch: u64,
        /// The stored epoch it failed to exceed.
        last_seen_epoch: u64,
    },
    /// The manifest version does not move forward over the installed
    /// version (downgrade or same-version).
    #[error("manifest version {manifest} does not exceed installed version {installed}")]
    VersionNotIncreasing {
        /// The version carried by the refused manifest.
        manifest: String,
        /// The installed version it failed to exceed.
        installed: String,
    },
    /// The installed version is older than the manifest's `min_version`;
    /// the install must update stepwise or be reinstalled manually.
    #[error("installed version {installed} is below the manifest minimum {min_version}")]
    BelowMinVersion {
        /// The manifest's declared minimum applicable version.
        min_version: String,
        /// The installed version that is too old.
        installed: String,
    },
    /// `valid_until` has passed.
    #[error("manifest expired at {valid_until} (now {now})")]
    Expired {
        /// The manifest's `valid_until`, Unix seconds.
        valid_until: i64,
        /// Local time at verification, Unix seconds.
        now: i64,
    },
    /// `valid_until` is more than 14 days after `issued_at`.
    #[error("manifest validity window exceeds 14 days (issued_at {issued_at}, valid_until {valid_until})")]
    ValidityWindowTooLong {
        /// The manifest's `issued_at`, Unix seconds.
        issued_at: i64,
        /// The manifest's `valid_until`, Unix seconds.
        valid_until: i64,
    },
    /// `issued_at` is more than 1 day ahead of local time: clock skew.
    #[error("manifest issued_at {issued_at} is more than 1 day ahead of local time {now}; check the system clock")]
    IssuedTooFarAhead {
        /// The manifest's `issued_at`, Unix seconds.
        issued_at: i64,
        /// Local time at verification, Unix seconds.
        now: i64,
    },
}

impl Manifest {
    /// Parse a manifest from its published JSON bytes. Unknown fields are
    /// refused: the signed format is closed, and anything extra could not
    /// be reproduced in the canonical bytes.
    pub fn from_json(bytes: &[u8]) -> Result<Self, Refusal> {
        serde_json::from_slice(bytes).map_err(|e| Refusal::Malformed(e.to_string()))
    }

    /// The canonical bytes the signatures cover: JSON with sorted keys
    /// and no whitespace, over every field except `signatures`. Built by
    /// hand in sorted key order so the encoding is exact and independent
    /// of serializer map ordering.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = String::new();
        out.push('{');
        // "files"
        out.push_str("\"files\":[");
        for (i, f) in self.files.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("{\"path\":");
            push_json_string(&mut out, &f.path);
            out.push_str(",\"sha256\":");
            push_json_string(&mut out, &f.sha256);
            out.push_str(",\"size\":");
            out.push_str(&f.size.to_string());
            out.push('}');
        }
        out.push_str("],\"issued_at\":");
        out.push_str(&self.issued_at.to_string());
        out.push_str(",\"manifest_epoch\":");
        out.push_str(&self.manifest_epoch.to_string());
        out.push_str(",\"min_version\":");
        push_json_string(&mut out, &self.min_version);
        out.push_str(",\"urls\":[");
        for (i, u) in self.urls.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            push_json_string(&mut out, u);
        }
        out.push_str("],\"valid_until\":");
        out.push_str(&self.valid_until.to_string());
        out.push_str(",\"version\":");
        push_json_string(&mut out, &self.version);
        out.push('}');
        out.into_bytes()
    }

    /// Verify this manifest end to end, in the design's order of trust:
    /// well-formedness of the security fields, pin self-consistency,
    /// signature, epoch monotonicity, version monotonicity, then the
    /// freshness window. On success the caller must durably store
    /// [`VerifiedManifest::epoch`] before any payload download.
    pub fn verify(
        &self,
        trust: &TrustRoot,
        ctx: &VerifyContext<'_>,
    ) -> Result<VerifiedManifest, Refusal> {
        // Structural checks on the fields the later steps rely on.
        if self.issued_at < 0 || self.valid_until < 0 {
            return Err(Refusal::Malformed("negative timestamp".to_owned()));
        }
        for f in &self.files {
            if !is_lower_hex(&f.sha256, 64) {
                return Err(Refusal::Malformed(format!(
                    "file {} has a sha256 that is not 64 lowercase hex characters",
                    f.path
                )));
            }
        }
        let manifest_version = SemVer::parse(&self.version)
            .map_err(|e| Refusal::Malformed(format!("version: {e}")))?;
        let min_version = SemVer::parse(&self.min_version)
            .map_err(|e| Refusal::Malformed(format!("min_version: {e}")))?;
        let installed_version = SemVer::parse(ctx.installed_version)
            .map_err(|e| Refusal::Malformed(format!("installed version: {e}")))?;

        // The trust root itself must be the self-consistent compiled pin
        // set; a transcribed-wrong constant refuses everything.
        if trust.pin == update_pin::publisher_pin() && !update_pin::pins_self_consistent() {
            return Err(Refusal::PinsInconsistent);
        }

        // Signature: an entry must name the pinned key and verify over
        // the canonical bytes.
        if self.signatures.is_empty() {
            return Err(Refusal::Unsigned);
        }
        let entry = self
            .signatures
            .iter()
            .find(|s| s.key_id == trust.pin)
            .ok_or(Refusal::UnknownKey)?;
        let sig_bytes = hex::decode(&entry.signature).map_err(|_| Refusal::BadSignature)?;
        let public = signature::UnparsedPublicKey::new(&signature::ED25519, trust.public_key);
        public
            .verify(&self.canonical_bytes(), &sig_bytes)
            .map_err(|_| Refusal::BadSignature)?;

        // Epoch monotonicity: strictly greater than the stored epoch.
        // Epochs start at 1, so 0 never passes either.
        if self.manifest_epoch == 0 || self.manifest_epoch <= ctx.last_seen_epoch {
            return Err(Refusal::EpochNotIncreasing {
                manifest_epoch: self.manifest_epoch,
                last_seen_epoch: ctx.last_seen_epoch,
            });
        }

        // Version monotonicity and applicability.
        if manifest_version <= installed_version {
            return Err(Refusal::VersionNotIncreasing {
                manifest: self.version.clone(),
                installed: ctx.installed_version.to_owned(),
            });
        }
        if installed_version < min_version {
            return Err(Refusal::BelowMinVersion {
                min_version: self.min_version.clone(),
                installed: ctx.installed_version.to_owned(),
            });
        }

        // Freshness window: span at most 14 days, not yet expired, and
        // not issued implausibly far in the future.
        if self.valid_until - self.issued_at > MAX_VALIDITY_SECS {
            return Err(Refusal::ValidityWindowTooLong {
                issued_at: self.issued_at,
                valid_until: self.valid_until,
            });
        }
        if self.valid_until <= ctx.now_unix {
            return Err(Refusal::Expired {
                valid_until: self.valid_until,
                now: ctx.now_unix,
            });
        }
        if self.issued_at > ctx.now_unix + MAX_ISSUED_AHEAD_SECS {
            return Err(Refusal::IssuedTooFarAhead {
                issued_at: self.issued_at,
                now: ctx.now_unix,
            });
        }

        Ok(VerifiedManifest {
            manifest: self.clone(),
        })
    }
}

/// Append `s` to `out` as a JSON string literal (quoted, escaped). Uses
/// the JSON serializer for the string alone so escaping is exact.
fn push_json_string(out: &mut String, s: &str) {
    let encoded = serde_json::to_string(s).expect("serializing a string cannot fail");
    out.push_str(&encoded);
}

/// True when `s` is exactly `len` lowercase hex characters.
fn is_lower_hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// A semantic version, parsed strictly enough for monotonicity
/// comparisons (`MAJOR.MINOR.PATCH` with optional prerelease; build
/// metadata is accepted and ignored for precedence, per semver).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemVer {
    major: u64,
    minor: u64,
    patch: u64,
    pre: Vec<PreIdent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PreIdent {
    Num(u64),
    Text(String),
}

impl SemVer {
    /// Parse a semantic version string. Rejects anything that is not
    /// `MAJOR.MINOR.PATCH` numerics with an optional `-prerelease` and an
    /// optional `+build`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let (without_build, _build) = match text.split_once('+') {
            Some((head, _)) => (head, ()),
            None => (text, ()),
        };
        let (core, pre) = match without_build.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (without_build, None),
        };
        let mut parts = core.split('.');
        let major = parse_core_number(parts.next())?;
        let minor = parse_core_number(parts.next())?;
        let patch = parse_core_number(parts.next())?;
        if parts.next().is_some() {
            return Err(format!("too many core components in {text:?}"));
        }
        let mut pre_idents = Vec::new();
        if let Some(pre) = pre {
            if pre.is_empty() {
                return Err(format!("empty prerelease in {text:?}"));
            }
            for ident in pre.split('.') {
                if ident.is_empty() {
                    return Err(format!("empty prerelease identifier in {text:?}"));
                }
                if ident.chars().all(|c| c.is_ascii_digit()) {
                    pre_idents.push(PreIdent::Num(parse_core_number(Some(ident))?));
                } else if ident.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                    pre_idents.push(PreIdent::Text(ident.to_owned()));
                } else {
                    return Err(format!("bad prerelease identifier {ident:?}"));
                }
            }
        }
        Ok(Self {
            major,
            minor,
            patch,
            pre: pre_idents,
        })
    }
}

fn parse_core_number(part: Option<&str>) -> Result<u64, String> {
    let part = part.ok_or_else(|| "missing version component".to_owned())?;
    if part.is_empty() || !part.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("bad numeric component {part:?}"));
    }
    if part.len() > 1 && part.starts_with('0') {
        return Err(format!("leading zero in numeric component {part:?}"));
    }
    part.parse::<u64>()
        .map_err(|_| format!("numeric component {part:?} out of range"))
}

impl Ord for SemVer {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering::*;
        self.major
            .cmp(&other.major)
            .then_with(|| self.minor.cmp(&other.minor))
            .then_with(|| self.patch.cmp(&other.patch))
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Equal,
                // A release outranks any of its prereleases.
                (true, false) => Greater,
                (false, true) => Less,
                (false, false) => cmp_pre(&self.pre, &other.pre),
            })
    }
}

impl PartialOrd for SemVer {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

fn cmp_pre(a: &[PreIdent], b: &[PreIdent]) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    for (x, y) in a.iter().zip(b.iter()) {
        let ord = match (x, y) {
            (PreIdent::Num(m), PreIdent::Num(n)) => m.cmp(n),
            // Numeric identifiers rank below alphanumeric ones.
            (PreIdent::Num(_), PreIdent::Text(_)) => Less,
            (PreIdent::Text(_), PreIdent::Num(_)) => Greater,
            (PreIdent::Text(s), PreIdent::Text(t)) => s.cmp(t),
        };
        if ord != Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::rand::SystemRandom;
    use ring::signature::{Ed25519KeyPair, KeyPair};
    use sha2::{Digest, Sha256};

    const ISSUED: i64 = 1_700_000_000;
    const NOW: i64 = ISSUED + 86_400;

    struct TestKey {
        key_pair: Ed25519KeyPair,
        trust: TrustRoot,
    }

    fn test_key() -> TestKey {
        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).expect("generate keypair");
        let key_pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).expect("parse keypair");
        let mut public_key = [0u8; 32];
        public_key.copy_from_slice(key_pair.public_key().as_ref());
        let pin = hex::encode(Sha256::digest(public_key));
        TestKey {
            key_pair,
            trust: TrustRoot { public_key, pin },
        }
    }

    fn base_manifest() -> Manifest {
        Manifest {
            version: "0.2.0".to_owned(),
            manifest_epoch: 7,
            issued_at: ISSUED,
            valid_until: ISSUED + 7 * 86_400,
            files: vec![ManifestFile {
                path: "udm.exe".to_owned(),
                sha256: "a".repeat(64),
                size: 123,
            }],
            min_version: "0.1.0".to_owned(),
            urls: vec!["https://example.invalid/udm.zip".to_owned()],
            signatures: Vec::new(),
        }
    }

    fn sign(mut manifest: Manifest, key: &TestKey) -> Manifest {
        let sig = key.key_pair.sign(&manifest.canonical_bytes());
        manifest.signatures = vec![ManifestSignature {
            key_id: key.trust.pin.clone(),
            signature: hex::encode(sig.as_ref()),
        }];
        manifest
    }

    fn ctx() -> VerifyContext<'static> {
        VerifyContext {
            now_unix: NOW,
            last_seen_epoch: 6,
            installed_version: "0.1.5",
        }
    }

    #[test]
    fn valid_manifest_accepted() {
        let key = test_key();
        let manifest = sign(base_manifest(), &key);
        let verified = manifest.verify(&key.trust, &ctx()).expect("valid manifest");
        assert_eq!(verified.epoch(), 7);
        assert_eq!(verified.manifest().version, "0.2.0");
    }

    #[test]
    fn canonical_bytes_are_sorted_and_compact() {
        let manifest = base_manifest();
        let expected = concat!(
            "{\"files\":[{\"path\":\"udm.exe\",\"sha256\":\"",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "\",\"size\":123}],\"issued_at\":1700000000,\"manifest_epoch\":7,",
            "\"min_version\":\"0.1.0\",\"urls\":[\"https://example.invalid/udm.zip\"],",
            "\"valid_until\":1700604800,\"version\":\"0.2.0\"}"
        );
        assert_eq!(manifest.canonical_bytes(), expected.as_bytes());
    }

    #[test]
    fn json_round_trip() {
        let key = test_key();
        let manifest = sign(base_manifest(), &key);
        let sig = &manifest.signatures[0];
        let text = format!(
            "{{\"version\":\"0.2.0\",\"manifest_epoch\":7,\"issued_at\":{ISSUED},\
             \"valid_until\":{},\"files\":[{{\"path\":\"udm.exe\",\"sha256\":\"{}\",\"size\":123}}],\
             \"min_version\":\"0.1.0\",\"urls\":[\"https://example.invalid/udm.zip\"],\
             \"signatures\":[{{\"key_id\":\"{}\",\"signature\":\"{}\"}}]}}",
            ISSUED + 7 * 86_400,
            "a".repeat(64),
            sig.key_id,
            sig.signature
        );
        let parsed = Manifest::from_json(text.as_bytes()).expect("parse");
        assert_eq!(parsed, manifest);
        // And the parsed form verifies against the same signature, which
        // proves the canonical bytes are a function of the model, not of
        // the published field order.
        parsed.verify(&key.trust, &ctx()).expect("verify parsed");
    }

    #[test]
    fn unknown_field_is_malformed() {
        let text = "{\"version\":\"0.2.0\",\"manifest_epoch\":7,\"issued_at\":1,\
                    \"valid_until\":2,\"files\":[],\"min_version\":\"0.1.0\",\
                    \"urls\":[],\"signatures\":[],\"surprise\":true}";
        assert!(matches!(
            Manifest::from_json(text.as_bytes()),
            Err(Refusal::Malformed(_))
        ));
        assert!(matches!(
            Manifest::from_json(b"{not json"),
            Err(Refusal::Malformed(_))
        ));
    }

    #[test]
    fn tampered_after_signing_is_bad_signature() {
        let key = test_key();
        let mut manifest = sign(base_manifest(), &key);
        manifest.version = "0.2.1".to_owned();
        assert_eq!(
            manifest.verify(&key.trust, &ctx()),
            Err(Refusal::BadSignature)
        );
    }

    #[test]
    fn wrong_key_is_refused() {
        let key = test_key();
        let other = test_key();
        // Signed by another key, entry naming that key: no entry for ours.
        let manifest = sign(base_manifest(), &other);
        assert_eq!(
            manifest.verify(&key.trust, &ctx()),
            Err(Refusal::UnknownKey)
        );
        // Signed by another key but claiming our key_id: does not verify.
        let mut manifest = sign(base_manifest(), &other);
        manifest.signatures[0].key_id = key.trust.pin.clone();
        assert_eq!(
            manifest.verify(&key.trust, &ctx()),
            Err(Refusal::BadSignature)
        );
    }

    #[test]
    fn unsigned_is_refused() {
        let key = test_key();
        let manifest = base_manifest();
        assert_eq!(manifest.verify(&key.trust, &ctx()), Err(Refusal::Unsigned));
    }

    #[test]
    fn epoch_must_strictly_increase() {
        let key = test_key();
        let manifest = sign(base_manifest(), &key);
        let mut replay_ctx = ctx();
        replay_ctx.last_seen_epoch = 7;
        assert!(matches!(
            manifest.verify(&key.trust, &replay_ctx),
            Err(Refusal::EpochNotIncreasing { .. })
        ));
        replay_ctx.last_seen_epoch = 8;
        assert!(matches!(
            manifest.verify(&key.trust, &replay_ctx),
            Err(Refusal::EpochNotIncreasing { .. })
        ));
        // Epoch 0 is never valid; epochs start at 1.
        let mut zero = base_manifest();
        zero.manifest_epoch = 0;
        let zero = sign(zero, &key);
        let mut fresh_ctx = ctx();
        fresh_ctx.last_seen_epoch = 0;
        assert!(matches!(
            zero.verify(&key.trust, &fresh_ctx),
            Err(Refusal::EpochNotIncreasing { .. })
        ));
    }

    #[test]
    fn version_must_increase_and_meet_minimum() {
        let key = test_key();
        let manifest = sign(base_manifest(), &key);
        let mut same = ctx();
        same.installed_version = "0.2.0";
        assert!(matches!(
            manifest.verify(&key.trust, &same),
            Err(Refusal::VersionNotIncreasing { .. })
        ));
        let mut newer = ctx();
        newer.installed_version = "0.3.0";
        assert!(matches!(
            manifest.verify(&key.trust, &newer),
            Err(Refusal::VersionNotIncreasing { .. })
        ));
        let mut too_old = ctx();
        too_old.installed_version = "0.0.9";
        assert!(matches!(
            manifest.verify(&key.trust, &too_old),
            Err(Refusal::BelowMinVersion { .. })
        ));
    }

    #[test]
    fn freshness_window_enforced() {
        let key = test_key();
        // Expired: now at valid_until.
        let manifest = sign(base_manifest(), &key);
        let mut late = ctx();
        late.now_unix = ISSUED + 7 * 86_400;
        assert!(matches!(
            manifest.verify(&key.trust, &late),
            Err(Refusal::Expired { .. })
        ));
        // Window longer than 14 days.
        let mut wide = base_manifest();
        wide.valid_until = ISSUED + 15 * 86_400;
        let wide = sign(wide, &key);
        assert!(matches!(
            wide.verify(&key.trust, &ctx()),
            Err(Refusal::ValidityWindowTooLong { .. })
        ));
        // Issued more than 1 day ahead of local time: clock skew.
        let mut future = base_manifest();
        future.issued_at = NOW + 2 * 86_400;
        future.valid_until = future.issued_at + 7 * 86_400;
        let future = sign(future, &key);
        assert!(matches!(
            future.verify(&key.trust, &ctx()),
            Err(Refusal::IssuedTooFarAhead { .. })
        ));
    }

    #[test]
    fn boundary_values_accepted() {
        let key = test_key();
        let mut manifest = base_manifest();
        manifest.issued_at = NOW + MAX_ISSUED_AHEAD_SECS;
        manifest.valid_until = manifest.issued_at + MAX_VALIDITY_SECS;
        let manifest = sign(manifest, &key);
        // Epoch exactly last_seen + 1 is the base context already.
        manifest
            .verify(&key.trust, &ctx())
            .expect("boundary manifest must be accepted");
    }

    #[test]
    fn bad_file_hash_is_malformed() {
        let key = test_key();
        let mut manifest = base_manifest();
        manifest.files[0].sha256 = "not-a-hash".to_owned();
        let manifest = sign(manifest, &key);
        assert!(matches!(
            manifest.verify(&key.trust, &ctx()),
            Err(Refusal::Malformed(_))
        ));
    }

    #[test]
    fn semver_ordering() {
        // The ordering example from semver.org, plus numeric width.
        let chain = [
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
            "1.0.1",
            "1.9.9",
            "1.10.0",
            "2.0.0",
        ];
        for pair in chain.windows(2) {
            let a = SemVer::parse(pair[0]).expect("parse a");
            let b = SemVer::parse(pair[1]).expect("parse b");
            assert!(a < b, "{} must be < {}", pair[0], pair[1]);
        }
        // Build metadata does not affect precedence.
        assert_eq!(
            SemVer::parse("1.0.0+build.42").expect("parse build"),
            SemVer::parse("1.0.0").expect("parse plain")
        );
        // Malformed versions are rejected.
        for bad in [
            "",
            "1.0",
            "1.0.0.0",
            "1.0.x",
            "01.0.0",
            "1.0.0-",
            "1.0.0-a..b",
        ] {
            assert!(SemVer::parse(bad).is_err(), "{bad:?} must not parse");
        }
    }
}
