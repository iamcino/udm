#!/usr/bin/env python3
"""U2 pin-consistency check (REV1 sec. 2.1 / 2.5).

Verifies, with no network and no toolchain beyond Python 3:
  1. The pin module's constants are well-formed: each public key is 32
     bytes, each pin is the 64-hex SHA-256 fingerprint of its key, hex and
     byte-array spellings agree, and publisher/recovery keys differ.
  2. The installer never carries its own pin: no literal pin or public-key
     hex appears in installer.nsi; the installer requires the build-time
     defines that build.ps1 passes.
  3. build.ps1 reads the pin from crates/core/src/update_pin.rs (the single
     source of truth) and passes it to makensis; it never generates or
     fetches a pin.
  4. `udm-cli update pin` is wired to the pin module.

Run by CI (see .github/workflows/ci.yml) and by the local/VM gates.
Exit 0 when every check passes, 1 with a message otherwise.
Usage: python3 tools/check-update-pin.py
"""

import hashlib
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PIN_MODULE = ROOT / "crates/core/src/update_pin.rs"
INSTALLER = ROOT / "packaging/windows/installer.nsi"
BUILD_PS1 = ROOT / "packaging/windows/build.ps1"
CLI_MAIN = ROOT / "crates/cli/src/main.rs"

FAILURES = []


def check(condition, message):
    print(("ok   " if condition else "FAIL ") + message)
    if not condition:
        FAILURES.append(message)


def read(path):
    return path.read_text(encoding="utf-8")


def hex_const(source, name):
    match = re.search(
        rf'pub const {name}: &str =\n\s*"([0-9a-f]{{64}})";', source
    )
    return match.group(1) if match else None


def byte_const(source, name):
    match = re.search(rf"pub const {name}: \[u8; 32\] = \[(.*?)\];", source, re.S)
    if not match:
        return None
    values = re.findall(r"0x([0-9a-f]{2})", match.group(1))
    return bytes(int(v, 16) for v in values) if len(values) == 32 else None


def main():
    pin_src = read(PIN_MODULE)
    for key_name, pin_name, hex_name in [
        ("PUBLISHER_PUBLIC_KEY", "PUBLISHER_PIN_SHA256", "PUBLISHER_PUBLIC_KEY_HEX"),
        ("RECOVERY_PUBLIC_KEY", "RECOVERY_PIN_SHA256", "RECOVERY_PUBLIC_KEY_HEX"),
    ]:
        key_bytes = byte_const(pin_src, key_name)
        pin = hex_const(pin_src, pin_name)
        key_hex = hex_const(pin_src, hex_name)
        check(key_bytes is not None, f"{key_name} is a 32-byte array")
        check(pin is not None, f"{pin_name} is 64 lowercase hex")
        check(key_hex is not None, f"{hex_name} is 64 lowercase hex")
        if key_bytes is not None and pin is not None:
            check(
                hashlib.sha256(key_bytes).hexdigest() == pin,
                f"{pin_name} is the SHA-256 fingerprint of {key_name}",
            )
        if key_bytes is not None and key_hex is not None:
            check(key_bytes.hex() == key_hex, f"{key_name} bytes match {hex_name}")
        if pin is not None:
            check(len(pin) == 64, f"{pin_name} is 64 hex, not a 40-hex thumbprint")

    pub_bytes = byte_const(pin_src, "PUBLISHER_PUBLIC_KEY")
    rec_bytes = byte_const(pin_src, "RECOVERY_PUBLIC_KEY")
    pub_pin = hex_const(pin_src, "PUBLISHER_PIN_SHA256")
    rec_pin = hex_const(pin_src, "RECOVERY_PIN_SHA256")
    if pub_bytes is not None and rec_bytes is not None:
        check(pub_bytes != rec_bytes, "publisher and recovery keys differ")
    if pub_pin is not None and rec_pin is not None:
        check(pub_pin != rec_pin, "publisher and recovery pins differ")

    nsi = read(INSTALLER)
    check("!ifndef UDM_UPDATE_PIN" in nsi, "installer.nsi requires UDM_UPDATE_PIN define")
    check(
        "!ifndef UDM_UPDATE_PUBKEY_HEX" in nsi,
        "installer.nsi requires UDM_UPDATE_PUBKEY_HEX define",
    )
    for value, label in [
        (pub_pin, "publisher pin"),
        (rec_pin, "recovery pin"),
        (hex_const(pin_src, "PUBLISHER_PUBLIC_KEY_HEX"), "publisher public key"),
        (hex_const(pin_src, "RECOVERY_PUBLIC_KEY_HEX"), "recovery public key"),
    ]:
        if value is not None:
            check(value not in nsi, f"installer.nsi carries no literal {label}")

    ps1 = read(BUILD_PS1)
    check(
        "crates\\core\\src\\update_pin.rs" in ps1 or "crates/core/src/update_pin.rs" in ps1,
        "build.ps1 reads the pin from crates/core/src/update_pin.rs",
    )
    check("PUBLISHER_PIN_SHA256" in ps1, "build.ps1 extracts PUBLISHER_PIN_SHA256")
    check(
        "PUBLISHER_PUBLIC_KEY_HEX" in ps1, "build.ps1 extracts PUBLISHER_PUBLIC_KEY_HEX"
    )
    check("/DUDM_UPDATE_PIN=" in ps1, "build.ps1 passes /DUDM_UPDATE_PIN= to makensis")
    check(
        "/DUDM_UPDATE_PUBKEY_HEX=" in ps1,
        "build.ps1 passes /DUDM_UPDATE_PUBKEY_HEX= to makensis",
    )

    cli = read(CLI_MAIN)
    check("UpdateCommand::Pin" in cli, "udm-cli wires the update pin subcommand")
    check(
        "update_pin::publisher_pin()" in cli, "udm-cli update pin prints the pin module value"
    )
    check(
        "pins_self_consistent()" in cli,
        "udm-cli update pin refuses to print on a failed pin self-check",
    )

    if FAILURES:
        print(f"\n{len(FAILURES)} check(s) failed")
        return 1
    print("\nupdate-pin consistency: all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
