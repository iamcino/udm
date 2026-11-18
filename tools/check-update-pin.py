#!/usr/bin/env python3
"""Update pin-consistency check (Yeager-style updater, 0.2.4).

Verifies, with no network and no toolchain beyond Python 3:
  1. The compiled pin in crates/core/src/updater.rs is well-formed
     (64 lowercase hex) and is the SHA-256 of the shipped signing
     certificate's SubjectPublicKeyInfo DER (the test vector below is
     the SPKI of the certificate published as `UDM-codesign.cer` with
     release v0.2.3; the signer embedded in the shipped `udm.exe` of
     that release is byte-identical).
  2. The installer never carries its own pin: no literal pin appears
     in installer.nsi; the installer requires the build-time define
     that build.ps1 passes, and writes it to update-pin.txt.
  3. build.ps1 reads the pin from crates/core/src/updater.rs (the
     single source of truth) and passes it to makensis; it never
     generates or fetches a pin.
  4. `udm-cli update pin` prints the compiled pin constant.

Run by CI (see .github/workflows/ci.yml) and by the local/VM gates.
Exit 0 when every check passes, 1 with a message otherwise.
Usage: python3 tools/check-update-pin.py
"""

import hashlib
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
UPDATER = ROOT / "crates/core/src/updater.rs"
INSTALLER = ROOT / "packaging/windows/installer.nsi"
BUILD_PS1 = ROOT / "packaging/windows/build.ps1"
CLI_MAIN = ROOT / "crates/cli/src/main.rs"

# DER SubjectPublicKeyInfo of the UDM code-signing certificate (EC
# P-256, 91 bytes), from the published UDM-codesign.cer of v0.2.3.
SHIPPED_SPKI_DER_HEX = (
    "3059301306072a8648ce3d020106082a8648ce3d03010703420004"
    "ee0e1c835ee768e6e2085a35195ddbb8f9963b76f2e036d9e853ce80b5963da"
    "554de42c1fc20e5ad3f484335d1a81a09b031e56954cc27ba6167cc7894f8dd06"
)

FAILURES = []


def check(condition, message):
    print(("ok   " if condition else "FAIL ") + message)
    if not condition:
        FAILURES.append(message)


def read(path):
    return path.read_text(encoding="utf-8")


def main():
    src = read(UPDATER)
    match = re.search(
        r'pub const PINNED_SPKI_SHA256_HEX: &str\s*=\s*"([0-9a-f]{64})";', src
    )
    pin = match.group(1) if match else None
    check(pin is not None, "PINNED_SPKI_SHA256_HEX is 64 lowercase hex in updater.rs")
    check(
        "decode_hex32(PINNED_SPKI_SHA256_HEX)" in src,
        "the byte form of the pin is derived from the hex constant",
    )
    if pin is not None:
        digest = hashlib.sha256(bytes.fromhex(SHIPPED_SPKI_DER_HEX)).hexdigest()
        check(
            digest == pin,
            "the pin is the SHA-256 of the shipped signing certificate's SPKI",
        )
        check(len(pin) == 64, "the pin is 64 hex, not a 40-hex thumbprint")

    nsi = read(INSTALLER)
    check("!ifndef UDM_SPKI_PIN" in nsi, "installer.nsi requires UDM_SPKI_PIN define")
    check(
        "signing_key_pin_spki_sha256=${UDM_SPKI_PIN}" in nsi,
        "installer.nsi writes the pin to update-pin.txt",
    )
    if pin is not None:
        check(pin not in nsi, "installer.nsi carries no literal pin")

    ps1 = read(BUILD_PS1)
    check(
        "crates\\core\\src\\updater.rs" in ps1 or "crates/core/src/updater.rs" in ps1,
        "build.ps1 reads the pin from crates/core/src/updater.rs",
    )
    check("PINNED_SPKI_SHA256_HEX" in ps1, "build.ps1 extracts PINNED_SPKI_SHA256_HEX")
    check("/DUDM_SPKI_PIN=" in ps1, "build.ps1 passes /DUDM_SPKI_PIN= to makensis")

    cli = read(CLI_MAIN)
    check("UpdateCommand::Pin" in cli, "udm-cli wires the update pin subcommand")
    check(
        "updater::PINNED_SPKI_SHA256_HEX" in cli,
        "udm-cli update pin prints the compiled pin constant",
    )

    if FAILURES:
        print(f"\n{len(FAILURES)} check(s) failed")
        return 1
    print("\nupdate-pin consistency: all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
