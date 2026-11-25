# Offline checks for the portable-zip sidecar. No network, no publish.
# SPDX-License-Identifier: MIT OR Apache-2.0
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2

. (Join-Path $PSScriptRoot 'release-sidecar.ps1')

$dir = Join-Path ([IO.Path]::GetTempPath()) ("udm-sidecar-test-" + [guid]::NewGuid().ToString('n'))
New-Item -ItemType Directory -Force $dir | Out-Null
try {
    $zipName = 'udm-9.9.9-portable.zip'
    $zip = Join-Path $dir $zipName
    [IO.File]::WriteAllBytes($zip, [byte[]](1, 2, 3, 4, 5))
    $sidecar = New-PortableSidecar -ZipPath $zip -OutDir $dir
    if ((Split-Path $sidecar -Leaf) -ne "$zipName.sha256") { throw "sidecar name is not the versioned zip name" }
    Test-PortableSidecar -ZipPath $zip -SidecarPath $sidecar
    $text = [IO.File]::ReadAllText($sidecar)
    if (-not $text.StartsWith((Get-FileHash $zip -Algorithm SHA256).Hash.ToLower())) {
        throw "sidecar does not start with the zip digest"
    }

    $digit = if ($text[0] -eq '0') { '1' } else { '0' }
    $broken = $digit + $text.Substring(1)
    [IO.File]::WriteAllText($sidecar, $broken)
    $failed = $false
    try { Test-PortableSidecar -ZipPath $zip -SidecarPath $sidecar }
    catch { $failed = $_.Exception.Message -like '*does not match*' }
    if (-not $failed) { throw "a mismatched sidecar was accepted" }

    $name = "$zipName.sha256"
    Test-VersionedSidecarLink -Link @{ direct_asset_path = "/$name" } -SidecarName $name
    Test-VersionedSidecarLink -Link @{
        direct_asset_url = "https://gitlab.com/cino-group/udm/-/releases/v9.9.9/downloads/$name"
    } -SidecarName $name
    $shortened = $false
    try {
        Test-VersionedSidecarLink -Link @{ direct_asset_path = '/udm-setup.exe' } -SidecarName $name
    }
    catch { $shortened = $_.Exception.Message -like '*not the versioned path*' }
    if (-not $shortened) { throw "a shortened GitLab path was accepted" }

    $gitlab = Get-Content (Join-Path $PSScriptRoot 'release-gitlab.ps1') -Raw
    if ($gitlab -notmatch 'SHA256SUMS\.txt') { throw "SHA256SUMS.txt is no longer published" }
    if ($gitlab -notmatch 'New-PortableSidecar') { throw "the GitLab release does not generate the sidecar" }
    $github = Get-Content (Join-Path $PSScriptRoot 'release-github.ps1') -Raw
    if ($github -notmatch 'Test-PortableSidecar') { throw "the GitHub mirror does not verify the sidecar" }
    if ($github -notmatch 'Test-VersionedSidecarLink') { throw "the GitHub mirror does not check the versioned path" }
    Write-Host "sidecar rules ok"
}
finally {
    Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
}
