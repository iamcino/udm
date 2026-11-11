# Build + package udm on a Windows 11 machine.
#
# Produces:
#   dist\udm.exe                     (GUI, manifest embedded, asInvoker)
#   dist\udm-cli.exe                 (headless engine)
#   dist\udm-<version>-setup.exe     (per-user NSIS installer)
#   dist\udm-<version>-portable.zip  (drop-anywhere folder, portable mode)
#
# Prereqs: Rust (stable, x86_64-pc-windows-msvc), NSIS 3 (makensis on PATH),
# PowerShell 5+. No admin needed for any of this.

param(
    [ValidateSet("release", "debug")] [string]$Profile = "release",
    [switch]$SkipInstaller,
    # Use the checked-in crates/gui/src/slint_generated.rs instead of compiling
    # the .slint sources at build time (saves ~2 GB of RAM at the cost of one
    # manual regeneration step - see packaging/regenerate-slint.sh).
    [switch]$NoSlintCompile,
    # Sign udm.exe, udm-cli.exe and the installer with the pinned UDM certificate
    # (tools\signing\sign.ps1) before they are packaged: the certificate in
    # CurrentUser\My, or -PfxPath with UDM_SIGN_PFX_PASSWORD set.
    [switch]$Sign,
    [string]$PfxPath
)

$ErrorActionPreference = "Stop"
$root = Resolve-Path (Join-Path $PSScriptRoot "..\..")
$dist = Join-Path $root "dist"
# Version comes from the workspace Cargo.toml ([workspace.package]) so a
# version bump touches exactly one place.
$version = (Select-String -Path (Join-Path $root "Cargo.toml") -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
if (-not $version) { throw "could not read workspace version from Cargo.toml" }

# U2 pin (REV1 sec. 2.1/2.5): the single source of truth is the compiled
# constant in crates/core/src/update_pin.rs. The installer never generates
# or fetches a pin; build.ps1 reads it from the Rust source and passes it
# to makensis as build-time defines (installer.nsi refuses to build
# without them). tools/check-update-pin.py enforces the same in CI.
$pinSource = Join-Path $root "crates\core\src\update_pin.rs"
# rustfmt splits the 64-hex constants across two lines (the declaration
# line ends at `=`, the value sits on the next line), so match against
# the raw file text: \s spans the line break. This still reads the
# single-line spelling too. (U1-1: the old single-line Select-String
# patterns could not read either constant after cargo fmt split them.)
$pinText = Get-Content -Path $pinSource -Raw
$pinMatch = [regex]::Match($pinText, 'pub const PUBLISHER_PIN_SHA256: &str\s*=\s*"([0-9a-f]{64})"')
if (-not $pinMatch.Success) { throw "could not read PUBLISHER_PIN_SHA256 from crates/core/src/update_pin.rs" }
$updatePin = $pinMatch.Groups[1].Value
$pubkeyMatch = [regex]::Match($pinText, 'pub const PUBLISHER_PUBLIC_KEY_HEX: &str\s*=\s*"([0-9a-f]{64})"')
if (-not $pubkeyMatch.Success) { throw "could not read PUBLISHER_PUBLIC_KEY_HEX from crates/core/src/update_pin.rs" }
$updatePubkey = $pubkeyMatch.Groups[1].Value

New-Item -ItemType Directory -Force -Path $dist | Out-Null

$featureArgs = @()
if ($NoSlintCompile) { $featureArgs += "--no-default-features" }

Write-Host "==> cargo build ($Profile)"
if ($Profile -eq "release") {
    cargo build --release @featureArgs -p udm-gui -p udm-cli
    $bin = Join-Path $root "target\release"
} else {
    cargo build @featureArgs -p udm-gui -p udm-cli
    $bin = Join-Path $root "target\debug"
}
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

Copy-Item (Join-Path $bin "udm.exe")     (Join-Path $dist "udm.exe")     -Force
Copy-Item (Join-Path $bin "udm-cli.exe") (Join-Path $dist "udm-cli.exe") -Force
Copy-Item (Join-Path $root "LICENSE")      (Join-Path $dist "LICENSE.txt")   -Force
Copy-Item (Join-Path $root "docs\PRIVACY.md") (Join-Path $dist "PRIVACY.md") -Force
Copy-Item (Join-Path $root "crates\gui\vendor\cino-ui\LICENSE") (Join-Path $dist "CINO-UI-LICENSE.txt") -Force
Copy-Item (Join-Path $root "crates\gui\vendor\cino-ui\ui\fonts\OFL.txt") (Join-Path $dist "BE-VIETNAM-PRO-OFL.txt") -Force

function Sign-File($file) {
    $signArgs = @{ Path = $file; ProjectName = 'UDM' }
    if ($PfxPath) { $signArgs.PfxPath = $PfxPath }
    & (Join-Path $root "tools\signing\sign.ps1") @signArgs
}
if ($Sign) {
    Write-Host "==> signing executables"
    Sign-File (Join-Path $dist "udm.exe")
    Sign-File (Join-Path $dist "udm-cli.exe")
}

if (-not $SkipInstaller) {
    Write-Host "==> makensis"
    Push-Location (Join-Path $root "packaging\windows")
    makensis /V2 /DVERSION=$version /DUDM_UPDATE_PIN=$updatePin /DUDM_UPDATE_PUBKEY_HEX=$updatePubkey installer.nsi
    Pop-Location
    if ($LASTEXITCODE -ne 0) { throw "makensis failed" }
    if ($Sign) {
        Write-Host "==> signing installer"
        Sign-File (Join-Path $dist "udm-$version-setup.exe")
    }
}

Write-Host "==> portable zip"
$portable = Join-Path $env:TEMP "udm-portable"
Remove-Item -Recurse -Force $portable -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $portable | Out-Null
Copy-Item (Join-Path $dist "udm.exe")     $portable
Copy-Item (Join-Path $dist "udm-cli.exe") $portable
Copy-Item (Join-Path $dist "LICENSE.txt")   $portable
Copy-Item (Join-Path $dist "PRIVACY.md")    $portable
Copy-Item (Join-Path $dist "CINO-UI-LICENSE.txt") $portable
Copy-Item (Join-Path $dist "BE-VIETNAM-PRO-OFL.txt") $portable
# The marker switches udm into portable mode: data lives beside the exe.
New-Item -ItemType File -Force -Path (Join-Path $portable "udm.portable") | Out-Null
Compress-Archive -Path $portable -DestinationPath (Join-Path $dist "udm-$version-portable.zip") -Force

Write-Host "==> done. artifacts in $dist"
Get-ChildItem $dist | Format-Table Name, Length
