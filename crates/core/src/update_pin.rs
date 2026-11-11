//! U2 publisher pin (REV1 sec. 2.1): the compiled trust root for updates.
//!
//! A single compiled constant holds the publisher Ed25519 public key and its
//! SHA-256 fingerprint (the pin), plus the recovery public key and its pin.
//! The recovery key NEVER signs regular manifests; its sole purpose is
//! signing rotation and revocation records (REV1 sec. 2.1, U5).
//!
//! Provenance (O3): both keypairs were generated offline by Cino by his own
//! hand and handed to this seat via Garen (gateway ticket
//! `20261110T054547-c005f2`, 2026-11-10). The pins were cross-verified
//! independently by Garen and again by this seat: SHA-256 of each raw public
//! key equals its pin. The PRIVATE keys are never in this repository, never
//! on CI runners, and never pass through the gateway.
//!
//! NOT the trust root: the 40-hex value `9FEE47A0...` is the SHA-1 thumbprint
//! of the code-signing certificate (Authenticode identity only). It must
//! never enter this module (anchor ruling, Garen ticket
//! `20261109T175655-f09533`).
//!
//! This module performs no network activity, generates nothing and fetches
//! nothing. The installer build consumes these same constants at build time
//! (REV1 sec. 2.5; see `packaging/windows/build.ps1`); a CI check
//! (`tools/check-update-pin.py`) fails the build if the installer pin and
//! the binary pin differ. First-trust procedure: the user runs
//! `udm-cli update pin` and compares the printed fingerprint out of band
//! against the fingerprint published in the signed release notes.

use sha2::{Digest, Sha256};

/// Publisher Ed25519 public key, raw bytes (32 bytes).
pub const PUBLISHER_PUBLIC_KEY: [u8; 32] = [
    0x4f, 0x06, 0xbd, 0xaf, 0x79, 0xee, 0xef, 0x81, 0xcc, 0xbd, 0x9b, 0x03, 0xe0, 0x71, 0xfb, 0xc7,
    0x5d, 0x3a, 0xfc, 0xbf, 0xc6, 0x8e, 0xc7, 0xf5, 0xfd, 0x34, 0x91, 0x9b, 0x36, 0x5b, 0x8c, 0xe2,
];

/// Publisher Ed25519 public key, lowercase hex (64 characters).
pub const PUBLISHER_PUBLIC_KEY_HEX: &str =
    "4f06bdaf79eeef81ccbd9b03e071fbc75d3afcbfc68ec7f5fd34919b365b8ce2";

/// Publisher pin: SHA-256 fingerprint of [`PUBLISHER_PUBLIC_KEY`],
/// lowercase hex (64 characters). This is the update trust root.
pub const PUBLISHER_PIN_SHA256: &str =
    "56d4202f4c6c715ea14809e62bddd7b504ce4e78b9412c7ac076e3276ee46538";

/// Recovery Ed25519 public key, raw bytes (32 bytes). Signs rotation and
/// revocation records only, never regular manifests.
pub const RECOVERY_PUBLIC_KEY: [u8; 32] = [
    0x58, 0xd8, 0xa4, 0x55, 0xfb, 0x9c, 0xa3, 0xed, 0xbd, 0x9d, 0x3d, 0x58, 0x05, 0xf8, 0x67, 0x35,
    0x67, 0x37, 0x2f, 0xff, 0xea, 0x37, 0x2e, 0x99, 0xe3, 0x4e, 0xec, 0x19, 0x02, 0xe4, 0x64, 0x0f,
];

/// Recovery Ed25519 public key, lowercase hex (64 characters).
pub const RECOVERY_PUBLIC_KEY_HEX: &str =
    "58d8a455fb9ca3edbd9d3d5805f8673567372fffea372e99e34eec1902e4640f";

/// Recovery pin: SHA-256 fingerprint of [`RECOVERY_PUBLIC_KEY`],
/// lowercase hex (64 characters).
pub const RECOVERY_PIN_SHA256: &str =
    "43567989750f298cc6bab0180ba82ff3fdf6d943dc34892a302fd4a688448b3f";

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

fn fingerprint_hex(public_key: &[u8; 32]) -> String {
    hex::encode(Sha256::digest(public_key))
}

/// Self-consistency check for the compiled constants: each pin must equal
/// the SHA-256 fingerprint of its public key, and the two keypairs must be
/// distinct. A failure means the constants were transcribed wrong; the
/// caller must refuse to trust the pin (never silently proceed).
pub fn pins_self_consistent() -> bool {
    fingerprint_hex(&PUBLISHER_PUBLIC_KEY) == PUBLISHER_PIN_SHA256
        && hex::encode(PUBLISHER_PUBLIC_KEY) == PUBLISHER_PUBLIC_KEY_HEX
        && fingerprint_hex(&RECOVERY_PUBLIC_KEY) == RECOVERY_PIN_SHA256
        && hex::encode(RECOVERY_PUBLIC_KEY) == RECOVERY_PUBLIC_KEY_HEX
        && PUBLISHER_PUBLIC_KEY != RECOVERY_PUBLIC_KEY
        && PUBLISHER_PIN_SHA256 != RECOVERY_PIN_SHA256
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
        // The SHA-1 code-signing thumbprint is 40 hex characters (anchor
        // ruling); a pin is always the 64-hex SHA-256 fingerprint. Length
        // alone separates them, and the thumbprint literal never appears
        // in this module.
        assert_eq!(PUBLISHER_PIN_SHA256.len(), 64);
        assert_eq!(RECOVERY_PIN_SHA256.len(), 64);
        assert_ne!(PUBLISHER_PIN_SHA256.len(), 40);
        assert_ne!(RECOVERY_PIN_SHA256.len(), 40);
    }
}
