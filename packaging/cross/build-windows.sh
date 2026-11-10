#!/usr/bin/env bash
# Cross-build udm for Windows x64 from Linux/macOS (MinGW-w64).
#
# This produces real PE32+ executables that run on Windows 11 without
# elevation. The NSIS installer must still be built on Windows (or with the
# Linux build of makensis, which is also fine: `makensis installer.nsi`).
set -euo pipefail

cd "$(dirname "$0")/../.."

# Version comes from the workspace Cargo.toml ([workspace.package]) so a
# version bump touches exactly one place.
VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"

TARGET="${TARGET:-x86_64-pc-windows-gnu}"

command -v cargo >/dev/null || { echo "cargo not on PATH"; exit 1; }
if [[ "$TARGET" == *-windows-gnu ]]; then
    command -v x86_64-w64-mingw32-gcc >/dev/null || {
        echo "install mingw-w64 (apt: gcc-mingw-w64-x86-64) first"; exit 1; }
fi

echo "==> cargo build --release --target $TARGET"
# --no-default-features: use the checked-in crates/gui/src/slint_generated.rs
# instead of compiling the .slint sources at build time. Compiling
# i-slint-compiler (via the default `slint-compile` feature) needs ~2 GB RAM;
# drop the flag on machines that have it. Regenerate the checked-in file with
# packaging/regenerate-slint.sh whenever ui/*.slint changes.
cargo build --release --target "$TARGET" --no-default-features \
    -p udm-gui -p udm-cli

mkdir -p dist
cp "target/$TARGET/release/udm.exe"     dist/
cp "target/$TARGET/release/udm-cli.exe" dist/
cp LICENSE dist/LICENSE.txt
cp docs/PRIVACY.md dist/
cp crates/gui/vendor/cino-ui/LICENSE dist/CINO-UI-LICENSE.txt
cp crates/gui/vendor/cino-ui/ui/fonts/OFL.txt dist/BE-VIETNAM-PRO-OFL.txt

echo "==> portable zip"
( cd dist && rm -rf portable && mkdir portable \
  && cp udm.exe udm-cli.exe LICENSE.txt PRIVACY.md CINO-UI-LICENSE.txt BE-VIETNAM-PRO-OFL.txt portable/ \
  && : > portable/udm.portable \
  && zip -qr "udm-$VERSION-portable.zip" portable )

echo "==> artifacts:"
ls -la dist
