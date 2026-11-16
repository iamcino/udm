# Build + package udm on a Windows 11 machine.
#
# Produces:
#   dist\udm.exe                     (GUI, manifest embedded, asInvoker)
#   dist\udm-cli.exe                 (headless engine)
#   dist\udm-host.exe                (browser Native Messaging host)
#   dist\udm-<version>-setup.exe     (per-user NSIS installer)
#   dist\udm-<version>-portable.zip  (drop-anywhere folder, portable mode)
#   dist\udm-extension-<browser>-<version>.zip  (standalone extension zips;
#                               the Firefox zip is also bundled inside the
#                               installer and the portable zip)
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
    # Sign udm.exe, udm-cli.exe, udm-host.exe and the installer with the pinned UDM certificate
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

# Zip writing with byte-exact entry names. Compress-Archive writes
# subdirectory entries with '\' separators on Windows, which strict
# The source root is taken from Get-Item, never Resolve-Path: the two
# disagree on 8.3 short paths (a CI runner's TEMP is C:\Users\RUNNER~1\...),
# and a short-form base silently shifts every Substring cut, mangling all
# entry names. Get-Item and Get-ChildItem both return the long form.
# consumers cannot resolve (the updater verifies portable members
# verbatim against the signed manifest; Firefox resolves extension
# resources by '/' name - the 0.2.1 zips' icons\ entries were
# unresolvable and the icon rendered blank). Entries are written
# explicitly with '/' names, files only, in sorted order.
function Write-Zip($sourceDir, $zipPath) {
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    if (Test-Path $zipPath) { Remove-Item -Force $zipPath }
    $base = (Get-Item $sourceDir).FullName.TrimEnd('\') + '\'
    $zip = [System.IO.Compression.ZipFile]::Open($zipPath, [System.IO.Compression.ZipArchiveMode]::Create)
    try {
        foreach ($file in Get-ChildItem -Recurse -File $sourceDir | Sort-Object FullName) {
            if (-not $file.FullName.StartsWith($base, [StringComparison]::OrdinalIgnoreCase)) { throw "zip source file outside its root: $($file.FullName)" }
            $rel = $file.FullName.Substring($base.Length).Replace('\', '/')
            $entry = $zip.CreateEntry($rel, [System.IO.Compression.CompressionLevel]::Optimal)
            $entry.LastWriteTime = $file.LastWriteTime
            $inStream = [System.IO.File]::OpenRead($file.FullName)
            $outStream = $entry.Open()
            try { $inStream.CopyTo($outStream) } finally { $outStream.Dispose(); $inStream.Dispose() }
        }
    } finally { $zip.Dispose() }
    Assert-ZipLayout $zipPath $false
}

# Entry names exactly as stored: parse the central directory bytes.
# (Readers that normalize separators - .NET's FullName, Python's
# ZipInfo.filename - cannot testify here; the raw bytes can.)
function Get-ZipRawEntryNames($zipPath) {
    $bytes = [System.IO.File]::ReadAllBytes($zipPath)
    $eocd = -1
    for ($i = $bytes.Length - 22; $i -ge 0; $i--) {
        if ($bytes[$i] -eq 0x50 -and $bytes[$i + 1] -eq 0x4b -and $bytes[$i + 2] -eq 0x05 -and $bytes[$i + 3] -eq 0x06) { $eocd = $i; break }
    }
    if ($eocd -lt 0) { throw "not a zip file: $zipPath" }
    $count = [BitConverter]::ToUInt16($bytes, $eocd + 10)
    $pos = [BitConverter]::ToUInt32($bytes, $eocd + 16)
    $names = @()
    for ($n = 0; $n -lt $count; $n++) {
        if ($bytes[$pos] -ne 0x50 -or $bytes[$pos + 1] -ne 0x4b -or $bytes[$pos + 2] -ne 0x01 -or $bytes[$pos + 3] -ne 0x02) { throw "corrupt central directory in $zipPath" }
        $nameLen = [BitConverter]::ToUInt16($bytes, $pos + 28)
        $extraLen = [BitConverter]::ToUInt16($bytes, $pos + 30)
        $commentLen = [BitConverter]::ToUInt16($bytes, $pos + 32)
        $names += [Text.Encoding]::UTF8.GetString($bytes, $pos + 46, $nameLen)
        $pos += 46 + $nameLen + $extraLen + $commentLen
    }
    $names
}

# The zip layout gate: no backslash in any raw entry name, ever; the
# portable zip additionally keeps its executables at the root, where
# the updater's member verification looks for them.
function Assert-ZipLayout($zipPath, $portableRoot) {
    $names = Get-ZipRawEntryNames $zipPath
    foreach ($name in $names) {
        if ($name.Contains('\')) { throw "zip entry with a backslash name in $zipPath`: $name" }
    }
    if ($portableRoot) {
        foreach ($exe in @("udm.exe", "udm-cli.exe", "udm-host.exe")) {
            if ($names -notcontains $exe) { throw "portable zip is missing root entry $exe in $zipPath" }
        }
    }
}

New-Item -ItemType Directory -Force -Path $dist | Out-Null

$featureArgs = @()
if ($NoSlintCompile) { $featureArgs += "--no-default-features" }

Write-Host "==> cargo build ($Profile)"
if ($Profile -eq "release") {
    cargo build --release @featureArgs -p udm-gui -p udm-cli -p udm-host
    $bin = Join-Path $root "target\release"
} else {
    cargo build @featureArgs -p udm-gui -p udm-cli -p udm-host
    $bin = Join-Path $root "target\debug"
}
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

Copy-Item (Join-Path $bin "udm.exe")     (Join-Path $dist "udm.exe")     -Force
Copy-Item (Join-Path $bin "udm-cli.exe") (Join-Path $dist "udm-cli.exe") -Force
Copy-Item (Join-Path $bin "udm-host.exe") (Join-Path $dist "udm-host.exe") -Force
Copy-Item (Join-Path $root "LICENSE")      (Join-Path $dist "LICENSE.txt")   -Force
Copy-Item (Join-Path $root "docs\PRIVACY.md") (Join-Path $dist "PRIVACY.md") -Force
Copy-Item (Join-Path $root "crates\gui\vendor\cino-ui\LICENSE") (Join-Path $dist "CINO-UI-LICENSE.txt") -Force
Copy-Item (Join-Path $root "crates\gui\vendor\cino-ui\ui\fonts\OFL.txt") (Join-Path $dist "BE-VIETNAM-PRO-OFL.txt") -Force

# Browser extension bundles: one self-contained folder per browser, staged
# from the extension tree. The Chrome folder keeps manifest.json; the
# Firefox folder carries manifest.firefox.json AS manifest.json, and the
# other browser's manifest and the tests stay out. The folders ship in the
# installer and in the portable zip; the zips of the same folders are
# standalone release assets for users who only want the extension. The
# Firefox zip is additionally bundled at the extension root of the
# installer and the portable zip, because Firefox installs the file,
# not the folder.
$extSrc = Join-Path $root "extension"
$extFiles = @("background.js", "handoff.js", "options.html", "options.js", "popup.css", "popup.html", "popup.js", "strings.js", "README.md")
foreach ($browser in @("chrome", "firefox")) {
    $dest = Join-Path $dist "extension\$browser"
    New-Item -ItemType Directory -Force -Path (Join-Path $dest "icons") | Out-Null
    foreach ($f in $extFiles) { Copy-Item (Join-Path $extSrc $f) (Join-Path $dest $f) -Force }
    Copy-Item (Join-Path $extSrc "icons\*.png") (Join-Path $dest "icons") -Force
    $manifest = if ($browser -eq "firefox") { "manifest.firefox.json" } else { "manifest.json" }
    Copy-Item (Join-Path $extSrc $manifest) (Join-Path $dest "manifest.json") -Force
    Write-Zip $dest (Join-Path $dist "udm-extension-$browser-$version.zip")
}

# The Firefox build also ships AS a zip file inside the bundles: a
# folder cannot be installed in Firefox, the zip can (about:debugging,
# Load Temporary Add-on, select the file). Staged at the extension
# root, it rides into the installer and the portable zip with the
# folders; the Chrome folder remains the load-unpacked unit.
Copy-Item (Join-Path $dist "udm-extension-firefox-$version.zip") (Join-Path $dist "extension\udm-extension-firefox-$version.zip") -Force

function Sign-File($file) {
    $signArgs = @{ Path = $file; ProjectName = 'UDM' }
    if ($PfxPath) { $signArgs.PfxPath = $PfxPath }
    & (Join-Path $root "tools\signing\sign.ps1") @signArgs
}
if ($Sign) {
    Write-Host "==> signing executables"
    Sign-File (Join-Path $dist "udm.exe")
    Sign-File (Join-Path $dist "udm-cli.exe")
    Sign-File (Join-Path $dist "udm-host.exe")
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
Copy-Item (Join-Path $dist "udm-host.exe") $portable
Copy-Item (Join-Path $dist "LICENSE.txt")   $portable
Copy-Item (Join-Path $dist "PRIVACY.md")    $portable
Copy-Item (Join-Path $dist "CINO-UI-LICENSE.txt") $portable
Copy-Item (Join-Path $dist "BE-VIETNAM-PRO-OFL.txt") $portable
Copy-Item -Recurse (Join-Path $dist "extension") (Join-Path $portable "extension")
# The marker switches udm into portable mode: data lives beside the exe.
New-Item -ItemType File -Force -Path (Join-Path $portable "udm.portable") | Out-Null
# Root-level entries: the updater verifies members verbatim against
# the signed manifest and stages a candidate that needs bare
# udm.exe/udm-cli.exe at the zip root (payload contract, 0.2.1 e2e).
Write-Zip $portable (Join-Path $dist "udm-$version-portable.zip")
Assert-ZipLayout (Join-Path $dist "udm-$version-portable.zip") $true

Write-Host "==> done. artifacts in $dist"
Get-ChildItem $dist | Format-Table Name, Length
