//! U2 publisher pin (REV1 sec. 2.1): the compiled trust root for updates.
//!
//! A single compiled constant holds the publisher Ed25519 public key and its
//! SHA-256 fingerprint (the pin), plus the recovery public key and its pin.
//! The recovery key NEVER signs regular manifests; its sole purpose is
//! signing rotation and revocation records (REV1 sec. 2.1, U5).
//!
//! Provenance: the publisher and recovery keypairs were provisioned on
//! 2026-11-10 and re-provisioned on 2026-11-14 by the project owner. The
//! PRIVATE keys are never in this repository and never on CI runners.
//! Each pin equals the SHA-256 fingerprint of its raw public key.
//!
//! NOT the trust root: the 40-hex value `9FEE47A0...` is the SHA-1 thumbprint
//! of the code-signing certificate (Authenticode identity only). It must
//! never enter this module.
//!
//! The module also carries the compiled minimum accepted version for
//! update targets (`MIN_ACCEPTED_VERSION`): a manifest whose version is
//! below it is refused, whatever the installed version is. The floor is
//! absolute and publisher-independent: once a release raises it, the
//! builds below it can never be update targets again.
//!
//! This module performs no network activity, generates nothing and fetches
//! nothing. The installer build consumes these same constants at build time
//! (REV1 sec. 2.5; see `packaging/windows/build.ps1`); a CI check
//! (`tools/check-update-pin.py`) fails the build if the installer pin and
//! the binary pin differ. First-trust procedure: the user runs
//! `udm-cli update pin` and compares the printed fingerprint out of band
//! against the fingerprint published in the signed release notes.

use sha2::{Digest, Sha256};

use crate::update_manifest::SemVer;

/// Publisher Ed25519 public key, raw bytes (32 bytes).
pub const PUBLISHER_PUBLIC_KEY: [u8; 32] = [
    0xf2, 0x14, 0x9a, 0xe9, 0xe4, 0xc3, 0xf6, 0x35, 0x85, 0x65, 0x5a, 0x95, 0x7e, 0x96, 0xc7, 0xde,
    0x29, 0xd9, 0x9a, 0xe9, 0x24, 0xe3, 0x47, 0xa1, 0xf3, 0x4b, 0x5f, 0x6d, 0x8e, 0xf3, 0xca, 0x35,
];

/// Publisher Ed25519 public key, lowercase hex (64 characters).
pub const PUBLISHER_PUBLIC_KEY_HEX: &str =
    "f2149ae9e4c3f63585655a957e96c7de29d99ae924e347a1f34b5f6d8ef3ca35";

/// Publisher pin: SHA-256 fingerprint of [`PUBLISHER_PUBLIC_KEY`],
/// lowercase hex (64 characters). This is the update trust root.
pub const PUBLISHER_PIN_SHA256: &str =
    "da86f60bcae67a20f22ca12ec78fee3e4aab653bc504e0c853c60286c7f629df";

/// Recovery Ed25519 public key, raw bytes (32 bytes). Signs rotation and
/// revocation records only, never regular manifests.
pub const RECOVERY_PUBLIC_KEY: [u8; 32] = [
    0xd7, 0x21, 0x0a, 0xe9, 0x0f, 0x95, 0x67, 0x14, 0xeb, 0xb7, 0xab, 0xa2, 0xe0, 0x16, 0x47, 0x72,
    0xff, 0xeb, 0x55, 0x77, 0xe3, 0x49, 0x51, 0x41, 0xe5, 0x8a, 0xfa, 0x41, 0xaf, 0x37, 0x97, 0xdc,
];

/// Recovery Ed25519 public key, lowercase hex (64 characters).
pub const RECOVERY_PUBLIC_KEY_HEX: &str =
    "d7210ae90f956714ebb7aba2e0164772ffeb5577e3495141e58afa41af3797dc";

/// Recovery pin: SHA-256 fingerprint of [`RECOVERY_PUBLIC_KEY`],
/// lowercase hex (64 characters).
pub const RECOVERY_PIN_SHA256: &str =
    "b50f72a93e0bd36d4be7c0eb28b78d03053f810478a6666b025adc2a0efb1fc9";

/// Minimum accepted version for update targets: the oldest version
/// that may ever be an update target. A manifest whose version is
/// below this floor is refused, whatever the installed version is.
/// The floor is absolute: it does not move with the installed
/// version or with any field the manifest declares. Raising it is a
/// one-line change owned by the release that raises it.
pub const MIN_ACCEPTED_VERSION: &str = "0.1.0";

/// The publisher pin (SHA-256 fingerprint of the publisher public key).
pub fn publisher_pin() -> &'static str {
    PUBLISHER_PIN_SHA256
}

/// The publisher public key, lowercase hex.
pub fn publisher_public_key_hex() -> &'static str {
    PUBLISHER_PUBLIC_KEY_HEX
}

/// The publisher public key, raw bytes.
pub fn publisher_public_key() -> [u8; 32] {
    PUBLISHER_PUBLIC_KEY
}

/// The recovery pin (SHA-256 fingerprint of the recovery public key).
pub fn recovery_pin() -> &'static str {
    RECOVERY_PIN_SHA256
}

/// The recovery public key, lowercase hex.
pub fn recovery_public_key_hex() -> &'static str {
    RECOVERY_PUBLIC_KEY_HEX
}

/// The recovery public key, raw bytes.
pub fn recovery_public_key() -> [u8; 32] {
    RECOVERY_PUBLIC_KEY
}

/// The minimum accepted version for update targets.
pub fn min_accepted_version() -> &'static str {
    MIN_ACCEPTED_VERSION
}

fn fingerprint_hex(public_key: &[u8; 32]) -> String {
    hex::encode(Sha256::digest(public_key))
}

/// Self-consistency check for the compiled constants: each pin must equal
/// the SHA-256 fingerprint of its public key, the two keypairs must be
/// distinct, and the version floor must parse as a semantic version
/// under the crate's own [`SemVer`]. A failure means the constants were
/// transcribed wrong; the caller must refuse to trust the pin (never
/// silently proceed). An unparseable floor refuses everything, the
/// same way an inconsistent pin does.
pub fn pins_self_consistent() -> bool {
    fingerprint_hex(&PUBLISHER_PUBLIC_KEY) == PUBLISHER_PIN_SHA256
        && hex::encode(PUBLISHER_PUBLIC_KEY) == PUBLISHER_PUBLIC_KEY_HEX
        && fingerprint_hex(&RECOVERY_PUBLIC_KEY) == RECOVERY_PIN_SHA256
        && hex::encode(RECOVERY_PUBLIC_KEY) == RECOVERY_PUBLIC_KEY_HEX
        && PUBLISHER_PUBLIC_KEY != RECOVERY_PUBLIC_KEY
        && PUBLISHER_PIN_SHA256 != RECOVERY_PIN_SHA256
        && SemVer::parse(MIN_ACCEPTED_VERSION).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_match_their_public_keys() {
        assert!(pins_self_consistent());
        assert_eq!(PUBLISHER_PIN_SHA256.len(), 64);
        assert_eq!(RECOVERY_PIN_SHA256.len(), 64);
        assert_eq!(PUBLISHER_PUBLIC_KEY_HEX.len(), 64);
        assert_eq!(RECOVERY_PUBLIC_KEY_HEX.len(), 64);
    }

    #[test]
    fn pins_are_lowercase_hex() {
        for value in [
            PUBLISHER_PIN_SHA256,
            RECOVERY_PIN_SHA256,
            PUBLISHER_PUBLIC_KEY_HEX,
            RECOVERY_PUBLIC_KEY_HEX,
        ] {
            assert!(
                value
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "{value} is not lowercase hex"
            );
        }
    }

    #[test]
    fn publisher_and_recovery_keys_are_distinct() {
        assert_ne!(PUBLISHER_PUBLIC_KEY, RECOVERY_PUBLIC_KEY);
        assert_ne!(PUBLISHER_PIN_SHA256, RECOVERY_PIN_SHA256);
    }

    #[test]
    fn pins_are_not_a_40_hex_thumbprint() {
        // The SHA-1 code-signing thumbprint is 40 hex characters; a pin is
        // always the 64-hex SHA-256 fingerprint. Length alone separates
        // them, and the thumbprint literal never appears in this module.
        assert_eq!(PUBLISHER_PIN_SHA256.len(), 64);
        assert_eq!(RECOVERY_PIN_SHA256.len(), 64);
        assert_ne!(PUBLISHER_PIN_SHA256.len(), 40);
        assert_ne!(RECOVERY_PIN_SHA256.len(), 40);
    }

    #[test]
    fn version_floor_is_a_parseable_semver() {
        assert_eq!(MIN_ACCEPTED_VERSION, "0.1.0");
        assert_eq!(min_accepted_version(), MIN_ACCEPTED_VERSION);
        assert!(SemVer::parse(MIN_ACCEPTED_VERSION).is_ok());
        assert!(pins_self_consistent());
    }
}
