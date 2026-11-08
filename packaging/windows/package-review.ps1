# Build and package an immutable local review candidate from a clean commit.
param(
    [Parameter(Mandatory = $true)][string]$OutputDirectory,
    [string]$CargoPath = 'cargo'
)
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
Push-Location $repo
try {
    $commit = (& git rev-parse HEAD).Trim()
    if ($LASTEXITCODE -ne 0) { throw 'Cannot identify source commit' }
    if (& git status --porcelain) { throw 'Commit source changes before packaging' }
    $name = "udm-0.1.0-review-$($commit.Substring(0, 12))"
    $folder = Join-Path $OutputDirectory $name
    $zip = "$folder-portable.zip"
    if ((Test-Path -LiteralPath $folder) -or (Test-Path -LiteralPath $zip)) {
        throw "Candidate already exists: $folder; use a new output directory to repeat verification"
    }
    & $CargoPath build --release --target x86_64-pc-windows-msvc --no-default-features -p udm-gui -p udm-cli --locked --offline
    if ($LASTEXITCODE -ne 0) { throw 'Windows release build failed' }
    New-Item -ItemType Directory -Path $folder -Force | Out-Null
    $metadata = @(
        'UDM local review candidate; independent review pending.',
        "Source commit: $commit",
        "Packaged UTC: $([DateTime]::UtcNow.ToString('o'))",
        'Target: x86_64-pc-windows-msvc; release; no-default-features; locked offline build.',
        'Portable mode stores data beside the executables. This is not a stable release.'
    )
    $hashes = @{}
    foreach ($binary in @('udm.exe', 'udm-cli.exe')) {
        Copy-Item -LiteralPath "target\x86_64-pc-windows-msvc\release\$binary" -Destination $folder
        $hashes[$binary] = (Get-FileHash -LiteralPath (Join-Path $folder $binary) -Algorithm SHA256).Hash.ToLowerInvariant()
        $metadata += "$binary SHA256 $($hashes[$binary])"
    }
    Copy-Item -LiteralPath 'LICENSE' -Destination (Join-Path $folder 'LICENSE.txt')
    Copy-Item -LiteralPath 'docs\PRIVACY.md' -Destination $folder
    Copy-Item -LiteralPath 'crates\gui\vendor\cino-ui\LICENSE' -Destination (Join-Path $folder 'CINO-UI-LICENSE.txt')
    Copy-Item -LiteralPath 'crates\gui\vendor\cino-ui\ui\fonts\OFL.txt' -Destination (Join-Path $folder 'BE-VIETNAM-PRO-OFL.txt')
    New-Item -ItemType File -Path (Join-Path $folder 'udm.portable') | Out-Null
    [IO.File]::WriteAllLines((Join-Path $folder 'BUILD.txt'), $metadata, [Text.UTF8Encoding]::new($false))
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    [IO.Compression.ZipFile]::CreateFromDirectory($folder, $zip)
    # Verify the archived bytes, not just the staging directory.
    $archive = [IO.Compression.ZipFile]::OpenRead($zip)
    try {
        foreach ($binary in @('udm.exe', 'udm-cli.exe')) {
            $entry = $archive.GetEntry($binary)
            if (!$entry) { throw "Archive missing $binary" }
            $stream = $entry.Open()
            $sha = [Security.Cryptography.SHA256]::Create()
            try {
                $actual = [BitConverter]::ToString($sha.ComputeHash($stream)).Replace('-', '').ToLowerInvariant()
                if ($actual -ne $hashes[$binary]) { throw "Archive hash mismatch: $binary" }
            } finally { $stream.Dispose(); $sha.Dispose() }
        }
        $reader = [IO.StreamReader]::new($archive.GetEntry('BUILD.txt').Open())
        try {
            $archivedMetadata = $reader.ReadToEnd()
            foreach ($line in $metadata) {
                if (!$archivedMetadata.Contains($line)) { throw 'Archive metadata mismatch' }
            }
        } finally { $reader.Dispose() }
    } finally { $archive.Dispose() }
    $zipHash = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
    [IO.File]::WriteAllText("$zip.sha256", "$zipHash  $([IO.Path]::GetFileName($zip))`n", [Text.UTF8Encoding]::new($false))
    Write-Output "Verified candidate: $zip"
    Write-Output "SHA256: $zipHash"
} finally { Pop-Location }
