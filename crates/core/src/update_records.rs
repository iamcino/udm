//! U5 rotation and revocation records (U2 design REV1 section 2.1).
//!
//! A rotation record is signed by the current publisher key and names the
//! next publisher public key. A revocation record is signed by the recovery
//! key and names the compromised publisher pin. The recovery key never
//! signs a regular manifest. There is no record type that recovers from
//! compromise of both keys: that path is out of band only, and this module
//! does not grow one.
//!
//! `key_id` on these records is the signer pin: the lowercase hex SHA-256
//! of the Ed25519 public key. That is the identifier unit U2 already
//! matches on manifest signatures. The rotation record carries the new
//! public key itself (`new_public_key`, lowercase hex). The client computes
//! the new pin from those bytes and does not trust a separately declared pin.
//!
//! Canonical bytes are JSON, sorted keys, no whitespace, over every field
//! except `signatures`, built by hand so the encoding does not depend on
//! serializer map order.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use ring::signature;
use serde::{Deserialize, Serialize};

use crate::update_pin;

/// Minimum time from `issued_at` to `cutover_at`: 30 days (REV1 section 2.1).
pub const MIN_ROTATION_LEAD_SECS: i64 = 30 * 24 * 60 * 60;

/// Maximum dual-signature window after cutover.
///
/// REV1 says the window ends and then only the new key signs, and it does
/// not name a length. An unbounded window would leave the retiring key
/// trusted forever, so a record whose window is longer than 30 days is
/// refused.
pub const MAX_DUAL_WINDOW_SECS: i64 = 30 * 24 * 60 * 60;

/// How far `issued_at` may sit ahead of local time. Same bound as a
/// manifest: one day. REV1 states that bound for manifests and is silent
/// for these records.
const MAX_ISSUED_AHEAD_SECS: i64 = 24 * 60 * 60;

/// An Ed25519 public key and the pin computed from it.
///
/// The pin is always SHA-256 of the raw public key, lowercase hex. Callers
/// cannot supply a pin that disagrees with the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ed25519Identity {
    public_key: [u8; 32],
    pin: String,
}

impl Ed25519Identity {
    /// Build an identity from raw public-key bytes. The pin is computed.
    pub fn from_public_key(public_key: [u8; 32]) -> Self {
        Self {
            public_key,
            pin: fingerprint(&public_key),
        }
    }

    /// Build an identity from a key and a pin that must already be that
    /// key's fingerprint. A mismatch is refused.
    pub fn from_pinning(public_key: [u8; 32], pin: &str) -> Result<Self, RecordError> {
        let computed = fingerprint(&public_key);
        if computed != pin || !is_lower_hex(pin, 64) {
            return Err(RecordError::PinMismatch);
        }
        Ok(Self {
            public_key,
            pin: computed,
        })
    }

    /// The raw Ed25519 public key.
    pub fn public_key(&self) -> &[u8; 32] {
        &self.public_key
    }

    /// The pin: SHA-256 of the public key, lowercase hex.
    pub fn pin(&self) -> &str {
        &self.pin
    }

    fn is_compiled_recovery(&self) -> bool {
        self.public_key == update_pin::RECOVERY_PUBLIC_KEY
            || self.pin == update_pin::RECOVERY_PIN_SHA256
    }

    fn is_compiled_publisher(&self) -> bool {
        self.public_key == update_pin::PUBLISHER_PUBLIC_KEY
            || self.pin == update_pin::PUBLISHER_PIN_SHA256
    }
}

/// Why a rotation or revocation record was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecordError {
    /// The bytes are not a well-formed record of the expected kind.
    #[error("trust record is malformed: {0}")]
    Malformed(String),
    /// The record carries no signature.
    #[error("trust record carries no signature")]
    Unsigned,
    /// The signature does not verify, or is not a 64-byte Ed25519 signature.
    #[error("trust record signature does not verify")]
    BadSignature,
    /// The signature names a key other than the one required for this record.
    #[error("trust record is not signed by the required key")]
    WrongSigner,
    /// The recovery key signed a rotation, or a publisher key signed a revocation.
    #[error("recovery key signs only revocation records and never a publisher rotation")]
    RecoveryKeyForbidden,
    /// `cutover_at` is less than 30 days after `issued_at`.
    #[error("rotation cutover {cutover_at} is less than 30 days after announcement {issued_at}")]
    LeadTimeTooShort {
        /// Announcement time, Unix seconds.
        issued_at: i64,
        /// Declared cutover time, Unix seconds.
        cutover_at: i64,
    },
    /// `cutover_epoch` is not at least one epoch past `announced_epoch`.
    #[error(
        "rotation cutover epoch {cutover_epoch} is not at least one release past announced epoch {announced_epoch}"
    )]
    ReleaseCycleTooShort {
        /// Epoch the announcement is bound to.
        announced_epoch: u64,
        /// First epoch at which the new key may sign.
        cutover_epoch: u64,
    },
    /// The dual-signature window is empty or longer than 30 days.
    #[error(
        "rotation dual-signature window from {cutover_at} to {window_until} is empty or longer than 30 days"
    )]
    DualWindowInvalid {
        /// Window open, Unix seconds.
        cutover_at: i64,
        /// Window close, Unix seconds.
        window_until: i64,
    },
    /// The new publisher key is not a distinct non-recovery key.
    #[error("rotation successor key is refused: {0}")]
    SuccessorRejected(String),
    /// A revocation names the recovery key. Both-key compromise stays out of band.
    #[error("revocation cannot name the recovery key; both-key recovery is out of band only")]
    RevokesRecoveryKey,
    /// A declared pin does not match the public key.
    #[error("public key and pin do not match")]
    PinMismatch,
    /// `issued_at` is more than one day ahead of local time.
    #[error("trust record issued_at {issued_at} is more than 1 day ahead of local time {now}")]
    IssuedTooFarAhead {
        /// Record `issued_at`, Unix seconds.
        issued_at: i64,
        /// Local time, Unix seconds.
        now: i64,
    },
    /// The client first saw the announcement at or after its cutover.
    #[error(
        "rotation announcement was first seen at or after cutover {cutover_at} (now {now}); refusing to trust the new key; download a fresh installer manually"
    )]
    AnnouncementAfterCutover {
        /// Local time, Unix seconds.
        now: i64,
        /// The record's cutover time, Unix seconds.
        cutover_at: i64,
    },
    /// `announced_epoch` is not the stored epoch or the next one.
    #[error(
        "rotation announced_epoch {announced_epoch} is not the stored epoch {last_seen_epoch} or the next epoch"
    )]
    AnnouncedEpochUnbound {
        /// Epoch named by the record.
        announced_epoch: u64,
        /// Epoch this client has stored.
        last_seen_epoch: u64,
    },
    /// The record is not newer than the announcement already stored.
    #[error(
        "rotation issued_at {issued_at} does not replace the stored announcement issued at {stored_issued_at}"
    )]
    NotNewer {
        /// The refused record's `issued_at`.
        issued_at: i64,
        /// The stored announcement's `issued_at`.
        stored_issued_at: i64,
    },
    /// After cutover the retiring key must not name a different successor.
    #[error(
        "the retiring publisher key cannot announce a different successor after cutover has begun"
    )]
    RetiringKeyClosed,
    /// The on-disk trust state could not be read or written.
    #[error("updater trust state is unusable: {0}")]
    Unreadable(String),
    /// The on-disk trust state is a version this client does not know.
    #[error("updater trust state version {0} is not supported")]
    UnsupportedVersion(u32),
    /// The on-disk trust state was written for a different compiled publisher.
    #[error("updater trust state was written for a different publisher pin")]
    CompiledPinMismatch,
}

/// A parsed rotation record. Authenticity is [`RotationRecord::verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationRecord {
    /// Successor Ed25519 public key, raw bytes.
    pub successor: Ed25519Identity,
    /// Announcement time, Unix seconds.
    pub issued_at: i64,
    /// When the dual-signature window opens, Unix seconds.
    pub cutover_at: i64,
    /// When the dual-signature window closes, Unix seconds.
    pub window_until: i64,
    /// Manifest epoch the announcement is bound to.
    pub announced_epoch: u64,
    /// First manifest epoch at which the successor may authorize a manifest.
    pub cutover_epoch: u64,
    /// Pin the signature claims as its signer.
    pub signer_key_id: String,
    /// Ed25519 signature, lowercase hex.
    pub signature: String,
}

/// A parsed revocation record. Authenticity is [`RevocationRecord::verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevocationRecord {
    /// Pin of the compromised publisher key.
    pub key_id: String,
    /// Signing time, Unix seconds.
    pub issued_at: i64,
    /// Pin the signature claims. This must be the recovery pin.
    pub signer_key_id: String,
    /// Ed25519 signature, lowercase hex.
    pub signature: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignatureWire {
    key_id: String,
    signature: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RotationWire {
    kind: String,
    new_public_key: String,
    issued_at: i64,
    cutover_at: i64,
    window_until: i64,
    announced_epoch: u64,
    cutover_epoch: u64,
    signatures: Vec<SignatureWire>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RevocationWire {
    kind: String,
    key_id: String,
    issued_at: i64,
    signatures: Vec<SignatureWire>,
}

impl RotationRecord {
    /// Parse a rotation record. Unknown fields are refused. The signature
    /// is not checked here.
    pub fn from_json(bytes: &[u8]) -> Result<Self, RecordError> {
        let wire: RotationWire =
            serde_json::from_slice(bytes).map_err(|err| RecordError::Malformed(err.to_string()))?;
        if wire.kind != "rotation" {
            return Err(RecordError::Malformed(
                "record kind is not rotation".to_owned(),
            ));
        }
        let signature = one_signature(&wire.signatures)?;
        if !is_lower_hex(&wire.new_public_key, 64) {
            return Err(RecordError::Malformed(
                "new_public_key is not 64 lowercase hex characters".to_owned(),
            ));
        }
        let raw = hex::decode(&wire.new_public_key).map_err(|err| {
            RecordError::Malformed(format!("new_public_key will not decode: {err}"))
        })?;
        let mut public_key = [0u8; 32];
        public_key.copy_from_slice(&raw);
        Ok(Self {
            successor: Ed25519Identity::from_public_key(public_key),
            issued_at: wire.issued_at,
            cutover_at: wire.cutover_at,
            window_until: wire.window_until,
            announced_epoch: wire.announced_epoch,
            cutover_epoch: wire.cutover_epoch,
            signer_key_id: signature.key_id.clone(),
            signature: signature.signature.clone(),
        })
    }

    /// Canonical bytes the signature covers.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = String::new();
        out.push('{');
        out.push_str("\"announced_epoch\":");
        out.push_str(&self.announced_epoch.to_string());
        out.push_str(",\"cutover_at\":");
        out.push_str(&self.cutover_at.to_string());
        out.push_str(",\"cutover_epoch\":");
        out.push_str(&self.cutover_epoch.to_string());
        out.push_str(",\"issued_at\":");
        out.push_str(&self.issued_at.to_string());
        out.push_str(",\"kind\":\"rotation\",\"new_public_key\":");
        push_json_string(&mut out, &hex::encode(self.successor.public_key));
        out.push_str(",\"window_until\":");
        out.push_str(&self.window_until.to_string());
        out.push('}');
        out.into_bytes()
    }

    /// Authenticate the record and enforce the schedule that is a property
    /// of the record itself: 30-day lead, one release cycle, and a
    /// non-empty dual window of at most 30 days.
    ///
    /// `now_unix` enforces the one-day future skew on `issued_at`. Whether
    /// this client is still before cutover, and so allowed to adopt the
    /// announcement, is a client decision and is not made here.
    pub fn verify(&self, signer: &Ed25519Identity, now_unix: i64) -> Result<(), RecordError> {
        if signer.is_compiled_recovery() {
            return Err(RecordError::RecoveryKeyForbidden);
        }
        if self.signer_key_id == update_pin::RECOVERY_PIN_SHA256 {
            return Err(RecordError::RecoveryKeyForbidden);
        }
        if self.signer_key_id != signer.pin {
            return Err(RecordError::WrongSigner);
        }
        verify_ed25519(&signer.public_key, &self.canonical_bytes(), &self.signature)?;
        self.check_schedule()?;
        self.check_successor(signer)?;
        check_issued_skew(self.issued_at, now_unix)?;
        Ok(())
    }

    fn check_schedule(&self) -> Result<(), RecordError> {
        if self.issued_at < 0 || self.cutover_at < 0 || self.window_until < 0 {
            return Err(RecordError::Malformed("negative timestamp".to_owned()));
        }
        if self.announced_epoch == 0 {
            return Err(RecordError::Malformed(
                "announced_epoch starts at 1".to_owned(),
            ));
        }
        let earliest = self
            .issued_at
            .checked_add(MIN_ROTATION_LEAD_SECS)
            .ok_or_else(|| RecordError::Malformed("rotation timestamps overflow".to_owned()))?;
        if self.cutover_at < earliest {
            return Err(RecordError::LeadTimeTooShort {
                issued_at: self.issued_at,
                cutover_at: self.cutover_at,
            });
        }
        let next_epoch = self
            .announced_epoch
            .checked_add(1)
            .ok_or_else(|| RecordError::Malformed("announced_epoch overflows".to_owned()))?;
        if self.cutover_epoch < next_epoch {
            return Err(RecordError::ReleaseCycleTooShort {
                announced_epoch: self.announced_epoch,
                cutover_epoch: self.cutover_epoch,
            });
        }
        if self.window_until <= self.cutover_at {
            return Err(RecordError::DualWindowInvalid {
                cutover_at: self.cutover_at,
                window_until: self.window_until,
            });
        }
        let latest = self
            .cutover_at
            .checked_add(MAX_DUAL_WINDOW_SECS)
            .ok_or_else(|| RecordError::Malformed("rotation window overflows".to_owned()))?;
        if self.window_until > latest {
            return Err(RecordError::DualWindowInvalid {
                cutover_at: self.cutover_at,
                window_until: self.window_until,
            });
        }
        Ok(())
    }

    fn check_successor(&self, signer: &Ed25519Identity) -> Result<(), RecordError> {
        if self.successor.public_key == signer.public_key || self.successor.pin == signer.pin {
            return Err(RecordError::SuccessorRejected(
                "successor is the signing key".to_owned(),
            ));
        }
        if self.successor.is_compiled_recovery() {
            return Err(RecordError::SuccessorRejected(
                "successor is the recovery key".to_owned(),
            ));
        }
        Ok(())
    }
}

impl RevocationRecord {
    /// Parse a revocation record. Unknown fields are refused. The
    /// signature is not checked here.
    pub fn from_json(bytes: &[u8]) -> Result<Self, RecordError> {
        let wire: RevocationWire =
            serde_json::from_slice(bytes).map_err(|err| RecordError::Malformed(err.to_string()))?;
        if wire.kind != "revocation" {
            return Err(RecordError::Malformed(
                "record kind is not revocation".to_owned(),
            ));
        }
        if !is_lower_hex(&wire.key_id, 64) {
            return Err(RecordError::Malformed(
                "revoked key_id is not 64 lowercase hex characters".to_owned(),
            ));
        }
        let signature = one_signature(&wire.signatures)?;
        Ok(Self {
            key_id: wire.key_id,
            issued_at: wire.issued_at,
            signer_key_id: signature.key_id.clone(),
            signature: signature.signature.clone(),
        })
    }

    /// Canonical bytes the recovery signature covers.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = String::new();
        out.push('{');
        out.push_str("\"issued_at\":");
        out.push_str(&self.issued_at.to_string());
        out.push_str(",\"key_id\":");
        push_json_string(&mut out, &self.key_id);
        out.push_str(",\"kind\":\"revocation\"}");
        out.into_bytes()
    }

    /// Authenticate a revocation. The signer must be the recovery identity
    /// the caller holds. The named key must not be that recovery key, and
    /// must not be the compiled recovery pin: a recovery key cannot revoke
    /// itself, and both-key compromise has no in-band record.
    pub fn verify(&self, recovery: &Ed25519Identity, now_unix: i64) -> Result<(), RecordError> {
        if recovery.is_compiled_publisher() {
            return Err(RecordError::RecoveryKeyForbidden);
        }
        if self.signer_key_id != recovery.pin {
            return Err(RecordError::WrongSigner);
        }
        verify_ed25519(
            &recovery.public_key,
            &self.canonical_bytes(),
            &self.signature,
        )?;
        if self.issued_at < 0 {
            return Err(RecordError::Malformed("negative timestamp".to_owned()));
        }
        if self.key_id == recovery.pin || self.key_id == update_pin::RECOVERY_PIN_SHA256 {
            return Err(RecordError::RevokesRecoveryKey);
        }
        check_issued_skew(self.issued_at, now_unix)?;
        Ok(())
    }
}

const TRUST_FILE_VERSION: u32 = 1;

/// Which publisher keys may authorize a manifest right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustPhase {
    /// No rotation has been adopted. Only the compiled publisher key.
    CompiledOnly,
    /// A rotation is stored and cutover has not begun.
    AwaitingCutover,
    /// Cutover has begun and the dual-signature window is still open.
    DualWindow,
    /// The dual-signature window has closed. Only the successor key.
    SuccessorOnly,
    /// Every publisher key is revoked, or the window closed before the
    /// cutover epoch was reached.
    NoPublisher,
}

/// The publisher keys a manifest verifier should use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustDecision {
    /// A signature from one of these keys is enough to authorize.
    pub authorizing: Vec<Ed25519Identity>,
    /// Every manifest signature must name one of these keys and verify.
    /// A key listed here but not in [`Self::authorizing`] does not
    /// authorize a manifest by itself.
    pub known: Vec<Ed25519Identity>,
    /// Publisher pins this client has revoked.
    pub revoked: Vec<String>,
    /// Why the sets look the way they do.
    pub phase: TrustPhase,
    /// Successor pin when a rotation is stored.
    pub successor_pin: Option<String>,
    /// Whether a rotation announcement has been adopted.
    pub has_rotation: bool,
}

/// Why staging or applying a signed candidate is refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StagingRefusal {
    /// The candidate carries no signature key id.
    #[error("the payload carries no publisher signature")]
    Unsigned,
    /// A signature names a revoked publisher key.
    #[error("publisher key {key_id} is revoked")]
    Revoked {
        /// Revoked pin.
        key_id: String,
    },
    /// A signature names the recovery key.
    #[error("the recovery key never authorizes a manifest")]
    RecoveryKey {
        /// Recovery pin that appeared on the manifest.
        key_id: String,
    },
    /// No publisher key is currently trusted.
    #[error("no publisher key is trusted")]
    NoTrustedPublisher,
    /// The manifest's signer was never announced to this install.
    #[error(
        "this install has no rotation announcement for that publisher key; download a fresh installer manually"
    )]
    Unannounced,
    /// The successor signed before its cutover.
    #[error("that publisher key is announced but its cutover has not started")]
    CutoverNotStarted,
    /// A signature names a key this install does not currently trust.
    #[error("signature key {key_id} is not a publisher key this install currently trusts")]
    NotAccepted {
        /// Pin that was not trusted.
        key_id: String,
    },
}

#[derive(Clone, Debug)]
struct AdoptedRotation {
    retiring: Ed25519Identity,
    successor: Ed25519Identity,
    issued_at: i64,
    cutover_at: i64,
    window_until: i64,
    announced_epoch: u64,
    cutover_epoch: u64,
}

/// Publisher keys this install will trust, beyond the compiled pin.
///
/// A new key enters only through [`Self::accept_rotation`], and only when
/// that call happens strictly before the record's cutover. A client that
/// never adopts an announcement does not gain the new key at cutover.
#[derive(Clone, Debug)]
pub struct ClientTrust {
    compiled: Ed25519Identity,
    recovery_pins: Vec<String>,
    rotation: Option<AdoptedRotation>,
    revoked: BTreeSet<String>,
}

impl ClientTrust {
    /// Trust only `publisher`. Refuses the compiled recovery key: it is
    /// not a publisher.
    pub fn bootstrap(publisher: Ed25519Identity) -> Result<Self, RecordError> {
        if publisher.is_compiled_recovery() {
            return Err(RecordError::RecoveryKeyForbidden);
        }
        Ok(Self {
            compiled: publisher,
            recovery_pins: Vec::new(),
            rotation: None,
            revoked: BTreeSet::new(),
        })
    }

    /// Remember a recovery pin so a manifest signed by it is refused.
    ///
    /// The compiled recovery pin is always refused as well, even when this
    /// is never called.
    pub fn bind_recovery(&mut self, recovery: &Ed25519Identity) {
        if !self.recovery_pins.iter().any(|pin| pin == recovery.pin()) {
            self.recovery_pins.push(recovery.pin().to_owned());
        }
    }

    /// The compiled publisher pin this trust state was built from.
    pub fn compiled_pin(&self) -> &str {
        self.compiled.pin()
    }

    /// Whether a rotation announcement has been adopted.
    pub fn has_rotation(&self) -> bool {
        self.rotation.is_some()
    }

    /// The adopted successor pin, if a rotation is stored.
    pub fn successor_pin(&self) -> Option<&str> {
        self.rotation.as_ref().map(|rot| rot.successor.pin())
    }

    /// Revoked publisher pins, in pin order.
    pub fn revoked_pins(&self) -> Vec<String> {
        self.revoked.iter().cloned().collect()
    }

    /// Verify a rotation record and adopt it.
    ///
    /// Adoption requires every schedule check on the record, a signature by
    /// the current publisher (the compiled key, or the successor once
    /// cutover of the stored rotation has begun), `now_unix` strictly
    /// before the new cutover, and `announced_epoch` equal to
    /// `last_seen_epoch` or the next epoch. The same announcement adopted
    /// twice is a no-op. An older or different announcement does not
    /// replace a newer one.
    pub fn accept_rotation(
        &mut self,
        bytes: &[u8],
        now_unix: i64,
        last_seen_epoch: u64,
        recovery: &Ed25519Identity,
    ) -> Result<(), RecordError> {
        self.reject_publisher_as_recovery(recovery)?;
        let record = RotationRecord::from_json(bytes)?;
        if let Some(rot) = &self.rotation {
            if now_unix >= rot.cutover_at && record.signer_key_id == rot.retiring.pin() {
                return Err(RecordError::RetiringKeyClosed);
            }
        }
        let signer = self.required_rotation_signer(now_unix);
        record.verify(&signer, now_unix)?;
        if record.successor.pin() == recovery.pin()
            || record.successor.public_key() == recovery.public_key()
        {
            return Err(RecordError::SuccessorRejected(
                "successor is the recovery key".to_owned(),
            ));
        }
        if self.revoked.contains(record.successor.pin()) || self.revoked.contains(signer.pin()) {
            return Err(RecordError::SuccessorRejected(
                "the signer or the successor is revoked".to_owned(),
            ));
        }
        if now_unix >= record.cutover_at {
            return Err(RecordError::AnnouncementAfterCutover {
                now: now_unix,
                cutover_at: record.cutover_at,
            });
        }
        if !epoch_is_bound(record.announced_epoch, last_seen_epoch) {
            return Err(RecordError::AnnouncedEpochUnbound {
                announced_epoch: record.announced_epoch,
                last_seen_epoch,
            });
        }
        if let Some(existing) = &self.rotation {
            if same_rotation(existing, &record, &signer) {
                return Ok(());
            }
            if record.issued_at <= existing.issued_at {
                return Err(RecordError::NotNewer {
                    issued_at: record.issued_at,
                    stored_issued_at: existing.issued_at,
                });
            }
        }
        self.bind_recovery(recovery);
        tracing::warn!(
            successor_pin = %record.successor.pin(),
            cutover_at = record.cutover_at,
            "accepted a publisher rotation announcement"
        );
        self.rotation = Some(AdoptedRotation {
            retiring: signer,
            successor: record.successor,
            issued_at: record.issued_at,
            cutover_at: record.cutover_at,
            window_until: record.window_until,
            announced_epoch: record.announced_epoch,
            cutover_epoch: record.cutover_epoch,
        });
        Ok(())
    }

    /// Verify a revocation record and stop trusting the named publisher pin.
    ///
    /// The effect is immediate and permanent. Adopting the same revocation
    /// twice is a no-op. Nothing here installs a replacement key.
    pub fn accept_revocation(
        &mut self,
        bytes: &[u8],
        now_unix: i64,
        recovery: &Ed25519Identity,
    ) -> Result<(), RecordError> {
        self.reject_publisher_as_recovery(recovery)?;
        let record = RevocationRecord::from_json(bytes)?;
        record.verify(recovery, now_unix)?;
        self.bind_recovery(recovery);
        if self.revoked.contains(&record.key_id) {
            return Ok(());
        }
        tracing::warn!(
            key_id = %record.key_id,
            "publisher key revoked; the installed build will be kept"
        );
        self.revoked.insert(record.key_id);
        Ok(())
    }

    /// Publisher keys that may verify a manifest at `now_unix` for a
    /// manifest that claims `manifest_epoch`.
    ///
    /// Cutover begins only when both the clock has reached `cutover_at`
    /// and the manifest epoch has reached `cutover_epoch`. Until then the
    /// successor does not authorize. After `window_until` the retiring key
    /// does not authorize. A revoked pin is in neither set.
    pub fn decision(&self, now_unix: i64, manifest_epoch: u64) -> TrustDecision {
        let revoked: Vec<String> = self.revoked.iter().cloned().collect();
        let Some(rot) = &self.rotation else {
            let authorizing = self.unless_revoked(&self.compiled);
            let phase = if authorizing.is_empty() {
                TrustPhase::NoPublisher
            } else {
                TrustPhase::CompiledOnly
            };
            return TrustDecision {
                known: authorizing.clone(),
                authorizing,
                revoked,
                phase,
                successor_pin: None,
                has_rotation: false,
            };
        };
        let successor_pin = Some(rot.successor.pin().to_owned());
        if now_unix >= rot.window_until {
            let authorizing = if manifest_epoch >= rot.cutover_epoch {
                self.unless_revoked(&rot.successor)
            } else {
                Vec::new()
            };
            let phase = if authorizing.is_empty() {
                TrustPhase::NoPublisher
            } else {
                TrustPhase::SuccessorOnly
            };
            return TrustDecision {
                known: authorizing.clone(),
                authorizing,
                revoked,
                phase,
                successor_pin,
                has_rotation: true,
            };
        }
        if now_unix >= rot.cutover_at && manifest_epoch >= rot.cutover_epoch {
            let mut authorizing = self.unless_revoked(&rot.retiring);
            authorizing.extend(self.unless_revoked(&rot.successor));
            let phase = if authorizing.is_empty() {
                TrustPhase::NoPublisher
            } else {
                TrustPhase::DualWindow
            };
            return TrustDecision {
                known: authorizing.clone(),
                authorizing,
                revoked,
                phase,
                successor_pin,
                has_rotation: true,
            };
        }
        let authorizing = self.unless_revoked(&rot.retiring);
        let mut known = authorizing.clone();
        known.extend(self.unless_revoked(&rot.successor));
        let phase = if authorizing.is_empty() {
            TrustPhase::NoPublisher
        } else {
            TrustPhase::AwaitingCutover
        };
        TrustDecision {
            authorizing,
            known,
            revoked,
            phase,
            successor_pin,
            has_rotation: true,
        }
    }

    /// Whether a signed candidate may be staged or applied.
    ///
    /// Every signature key id must be a known publisher key that is not
    /// revoked, and at least one must currently authorize. The recovery
    /// key never passes.
    pub fn allows_staging(
        &self,
        now_unix: i64,
        manifest_epoch: u64,
        signature_key_ids: &[String],
    ) -> Result<(), StagingRefusal> {
        if signature_key_ids.is_empty() {
            return Err(StagingRefusal::Unsigned);
        }
        let mut seen = BTreeSet::new();
        for key_id in signature_key_ids {
            if !seen.insert(key_id) {
                return Err(StagingRefusal::NotAccepted {
                    key_id: key_id.clone(),
                });
            }
            if self.is_recovery_pin(key_id) {
                return Err(StagingRefusal::RecoveryKey {
                    key_id: key_id.clone(),
                });
            }
            if self.revoked.contains(key_id) {
                return Err(StagingRefusal::Revoked {
                    key_id: key_id.clone(),
                });
            }
        }
        let decision = self.decision(now_unix, manifest_epoch);
        if decision.authorizing.is_empty() {
            return Err(StagingRefusal::NoTrustedPublisher);
        }
        let mut authorized = false;
        for key_id in signature_key_ids {
            let known = decision.known.iter().any(|key| key.pin() == key_id);
            if !known {
                if !decision.has_rotation {
                    return Err(StagingRefusal::Unannounced);
                }
                if decision.phase == TrustPhase::AwaitingCutover
                    && decision.successor_pin.as_deref() == Some(key_id)
                {
                    return Err(StagingRefusal::CutoverNotStarted);
                }
                return Err(StagingRefusal::NotAccepted {
                    key_id: key_id.clone(),
                });
            }
            if decision.authorizing.iter().any(|key| key.pin() == key_id) {
                authorized = true;
            }
        }
        if !authorized {
            if decision.phase == TrustPhase::AwaitingCutover {
                return Err(StagingRefusal::CutoverNotStarted);
            }
            if !decision.has_rotation {
                return Err(StagingRefusal::Unannounced);
            }
            return Err(StagingRefusal::NotAccepted {
                key_id: signature_key_ids[0].clone(),
            });
        }
        Ok(())
    }

    /// Write this state so a later launch still enforces it.
    pub fn store(&self, path: &Path) -> Result<(), RecordError> {
        let file = TrustFile {
            version: TRUST_FILE_VERSION,
            compiled_pin: self.compiled.pin().to_owned(),
            rotation: self.rotation.as_ref().map(|rot| RotationFile {
                retiring_public_key: hex::encode(rot.retiring.public_key()),
                successor_public_key: hex::encode(rot.successor.public_key()),
                issued_at: rot.issued_at,
                cutover_at: rot.cutover_at,
                window_until: rot.window_until,
                announced_epoch: rot.announced_epoch,
                cutover_epoch: rot.cutover_epoch,
            }),
            revoked: self.revoked.iter().cloned().collect(),
        };
        let bytes =
            serde_json::to_vec(&file).map_err(|err| RecordError::Unreadable(err.to_string()))?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)
                .map_err(|err| RecordError::Unreadable(format!("{}: {err}", dir.display())))?;
        }
        let mut tmp_name = path.as_os_str().to_owned();
        tmp_name.push(".tmp");
        let tmp = std::path::PathBuf::from(tmp_name);
        {
            let mut out = File::create(&tmp)
                .map_err(|err| RecordError::Unreadable(format!("{}: {err}", tmp.display())))?;
            out.write_all(&bytes)
                .map_err(|err| RecordError::Unreadable(format!("{}: {err}", tmp.display())))?;
            out.sync_all()
                .map_err(|err| RecordError::Unreadable(format!("{}: {err}", tmp.display())))?;
        }
        fs::rename(&tmp, path)
            .map_err(|err| RecordError::Unreadable(format!("{}: {err}", path.display())))?;
        Ok(())
    }

    /// Replace the adopted rotation and revoked set from `path`.
    ///
    /// A missing file leaves this state unchanged. A file that does not
    /// parse, names another compiled pin, or breaks the rotation schedule
    /// is an error and leaves this state unchanged. The compiled publisher
    /// key is never taken from the file.
    pub fn load_into(&mut self, path: &Path) -> Result<(), RecordError> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(RecordError::Unreadable(format!(
                    "{}: {err}",
                    path.display()
                )))
            }
        };
        let file: TrustFile = serde_json::from_slice(&bytes)
            .map_err(|err| RecordError::Unreadable(err.to_string()))?;
        if file.version != TRUST_FILE_VERSION {
            return Err(RecordError::UnsupportedVersion(file.version));
        }
        if file.compiled_pin != self.compiled.pin() {
            return Err(RecordError::CompiledPinMismatch);
        }
        let rotation = match file.rotation {
            None => None,
            Some(rot) => Some(Self::parse_stored_rotation(rot)?),
        };
        let mut revoked = BTreeSet::new();
        for pin in file.revoked {
            if !is_lower_hex(&pin, 64) || !revoked.insert(pin) {
                return Err(RecordError::Unreadable(
                    "revoked pin list is not distinct lowercase pins".to_owned(),
                ));
            }
        }
        self.rotation = rotation;
        self.revoked = revoked;
        Ok(())
    }

    fn parse_stored_rotation(rot: RotationFile) -> Result<AdoptedRotation, RecordError> {
        let retiring = identity_from_hex(&rot.retiring_public_key)?;
        let successor = identity_from_hex(&rot.successor_public_key)?;
        let probe = RotationRecord {
            successor: successor.clone(),
            issued_at: rot.issued_at,
            cutover_at: rot.cutover_at,
            window_until: rot.window_until,
            announced_epoch: rot.announced_epoch,
            cutover_epoch: rot.cutover_epoch,
            signer_key_id: String::new(),
            signature: String::new(),
        };
        probe.check_schedule()?;
        if retiring.pin() == successor.pin() {
            return Err(RecordError::SuccessorRejected(
                "stored successor is the retiring key".to_owned(),
            ));
        }
        if successor.is_compiled_recovery() || retiring.is_compiled_recovery() {
            return Err(RecordError::RecoveryKeyForbidden);
        }
        Ok(AdoptedRotation {
            retiring,
            successor,
            issued_at: rot.issued_at,
            cutover_at: rot.cutover_at,
            window_until: rot.window_until,
            announced_epoch: rot.announced_epoch,
            cutover_epoch: rot.cutover_epoch,
        })
    }

    fn required_rotation_signer(&self, now_unix: i64) -> Ed25519Identity {
        match &self.rotation {
            None => self.compiled.clone(),
            Some(rot) if now_unix < rot.cutover_at => rot.retiring.clone(),
            Some(rot) => rot.successor.clone(),
        }
    }

    fn reject_publisher_as_recovery(&self, recovery: &Ed25519Identity) -> Result<(), RecordError> {
        if recovery.pin() == self.compiled.pin()
            || recovery.public_key() == self.compiled.public_key()
            || recovery.is_compiled_publisher()
        {
            return Err(RecordError::RecoveryKeyForbidden);
        }
        Ok(())
    }

    fn unless_revoked(&self, key: &Ed25519Identity) -> Vec<Ed25519Identity> {
        if self.revoked.contains(key.pin()) {
            Vec::new()
        } else {
            vec![key.clone()]
        }
    }

    fn is_recovery_pin(&self, pin: &str) -> bool {
        pin == update_pin::recovery_pin() || self.recovery_pins.iter().any(|stored| stored == pin)
    }
}

fn epoch_is_bound(announced_epoch: u64, last_seen_epoch: u64) -> bool {
    if announced_epoch == 0 {
        return false;
    }
    if announced_epoch == last_seen_epoch {
        return true;
    }
    matches!(last_seen_epoch.checked_add(1), Some(next) if announced_epoch == next)
}

fn same_rotation(
    existing: &AdoptedRotation,
    record: &RotationRecord,
    signer: &Ed25519Identity,
) -> bool {
    existing.retiring.pin() == signer.pin()
        && existing.successor.pin() == record.successor.pin()
        && existing.issued_at == record.issued_at
        && existing.cutover_at == record.cutover_at
        && existing.window_until == record.window_until
        && existing.announced_epoch == record.announced_epoch
        && existing.cutover_epoch == record.cutover_epoch
}

fn identity_from_hex(hex_key: &str) -> Result<Ed25519Identity, RecordError> {
    if !is_lower_hex(hex_key, 64) {
        return Err(RecordError::Unreadable(
            "stored public key is not 64 lowercase hex characters".to_owned(),
        ));
    }
    let raw = hex::decode(hex_key).map_err(|err| RecordError::Unreadable(err.to_string()))?;
    let mut public_key = [0u8; 32];
    public_key.copy_from_slice(&raw);
    Ok(Ed25519Identity::from_public_key(public_key))
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustFile {
    version: u32,
    compiled_pin: String,
    rotation: Option<RotationFile>,
    revoked: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RotationFile {
    retiring_public_key: String,
    successor_public_key: String,
    issued_at: i64,
    cutover_at: i64,
    window_until: i64,
    announced_epoch: u64,
    cutover_epoch: u64,
}

fn one_signature(signatures: &[SignatureWire]) -> Result<&SignatureWire, RecordError> {
    match signatures {
        [] => Err(RecordError::Unsigned),
        [one] => {
            if !is_lower_hex(&one.key_id, 64) || !is_lower_hex(&one.signature, 128) {
                return Err(RecordError::BadSignature);
            }
            Ok(one)
        }
        _ => Err(RecordError::Malformed(
            "trust record must carry exactly one signature".to_owned(),
        )),
    }
}

fn verify_ed25519(
    public_key: &[u8; 32],
    message: &[u8],
    signature_hex: &str,
) -> Result<(), RecordError> {
    let sig = hex::decode(signature_hex).map_err(|_| RecordError::BadSignature)?;
    if sig.len() != 64 {
        return Err(RecordError::BadSignature);
    }
    let key = signature::UnparsedPublicKey::new(&signature::ED25519, public_key);
    key.verify(message, &sig)
        .map_err(|_| RecordError::BadSignature)
}

fn check_issued_skew(issued_at: i64, now_unix: i64) -> Result<(), RecordError> {
    let limit = now_unix
        .checked_add(MAX_ISSUED_AHEAD_SECS)
        .ok_or_else(|| RecordError::Malformed("local time overflows".to_owned()))?;
    if issued_at > limit {
        return Err(RecordError::IssuedTooFarAhead {
            issued_at,
            now: now_unix,
        });
    }
    Ok(())
}

fn fingerprint(public_key: &[u8; 32]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(public_key))
}

fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

fn push_json_string(out: &mut String, value: &str) {
    let encoded = serde_json::to_string(value).expect("serializing a string cannot fail");
    out.push_str(&encoded);
}

#[cfg(test)]
#[path = "update_records_tests.rs"]
mod tests;
