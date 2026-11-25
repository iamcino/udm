# Shared sidecar rules for the GitLab and GitHub release scripts.
# The portable zip's .sha256 file is generated from the final zip, and the
# GitLab link keeps that versioned name as its direct path.
# SPDX-License-Identifier: MIT OR Apache-2.0

function New-PortableSidecar {
    param(
        [Parameter(Mandatory)][string]$ZipPath,
        [Parameter(Mandatory)][string]$OutDir
    )
    $name = Split-Path $ZipPath -Leaf
    $hash = (Get-FileHash $ZipPath -Algorithm SHA256).Hash.ToLower()
    $text = "$hash  $name`n"
    $sidecar = Join-Path $OutDir "$name.sha256"
    [IO.File]::WriteAllText($sidecar, $text)
    return $sidecar
}

function Test-PortableSidecar {
    param(
        [Parameter(Mandatory)][string]$ZipPath,
        [Parameter(Mandatory)][string]$SidecarPath
    )
    $text = [IO.File]::ReadAllText($SidecarPath)
    if ($text.Length -lt 64 -or $text.Substring(0, 64) -notmatch '^[0-9a-fA-F]{64}$') {
        throw "sidecar does not start with 64 hex characters"
    }
    $hex = $text.Substring(0, 64).ToLower()
    $actual = (Get-FileHash $ZipPath -Algorithm SHA256).Hash.ToLower()
    if ($hex -ne $actual) { throw "the sidecar does not match the portable zip" }
}

function Test-VersionedSidecarLink {
    param(
        [Parameter(Mandatory)]$Link,
        [Parameter(Mandatory)][string]$SidecarName
    )
    $path = $null
    if ($Link -is [System.Collections.IDictionary]) {
        if ($Link.Contains('direct_asset_path')) { $path = $Link['direct_asset_path'] }
        elseif ($Link.Contains('direct_asset_url')) { $path = $Link['direct_asset_url'] }
    } else {
        if ($Link.PSObject.Properties.Name -contains 'direct_asset_path' -and $Link.direct_asset_path) {
            $path = $Link.direct_asset_path
        } elseif ($Link.PSObject.Properties.Name -contains 'direct_asset_url') {
            $path = $Link.direct_asset_url
        }
    }
    if (-not $path) { throw "GitLab sidecar link has no direct path" }
    $leaf = ([string]$path).TrimEnd('/') -split '/' | Select-Object -Last 1
    if ($leaf -ne $SidecarName) {
        throw "GitLab sidecar link is not the versioned path: $path"
    }
}
