<#
.SYNOPSIS
  Builds and publishes a UDM release on GitLab (cino-group/udm): the per-user installer, the
  portable zip, SHA256SUMS.txt and LICENSE, with release notes from the changelog.

.DESCRIPTION
  Run it from a clone of the public GitLab repository (or inside GitLab CI), never from the
  private GitHub checkout: a release is always built from exactly the source that is public on
  GitLab. Publish the snapshot first with tools\publish-gitlab-snapshot.ps1, then release that
  commit. The script refuses a tree that still contains the private agent documents, and a
  commit that is not on GitLab main.

  Needs Windows with Rust (MSVC toolchain), the MSVC build tools and NSIS 3. The build runs in
  a clean temporary worktree of -Ref, so your checkout is not touched. The version is the
  workspace version in Cargo.toml; the tag is v<version> and GitLab creates it with the release.

  Release notes: -NotesFile, or the "## <version>" section of CHANGELOG.md (and CHANGELOG.vi.md
  when it exists).

  Signing is required. udm.exe, udm-cli.exe and the installer are signed with the pinned UDM
  certificate (docs\signing\UDM-codesign.cer) and checked against its thumbprint; the .cer and
  signature reports are published with the release. Key, first match wins:
    1. the pinned certificate in Cert:\CurrentUser\My
    2. -PfxPath, or <repo>\.signing\UDM-codesign.pfx, with the password from
       UDM_SIGN_PFX_PASSWORD or %LOCALAPPDATA%\UDM-signing\pfx-password.clixml (DPAPI),
       or asked for when the script runs
  -AllowUnsigned skips signing. Use it only when the owner approved an unsigned release.

  GitLab access: a project access token (scope: api) from GITLAB_TOKEN, from
  %LOCALAPPDATA%\UDM-release\gitlab-token.clixml (DPAPI), the CI job token inside GitLab CI, or
  asked for when the script starts. The script never writes it anywhere.

.EXAMPLE
  .\tools\release-gitlab.ps1 -DryRun                      # build and package main; nothing is uploaded
.EXAMPLE
  .\tools\release-gitlab.ps1                              # build and publish GitLab main's version
.EXAMPLE
  .\tools\release-gitlab.ps1 -NotesFile D:\notes-0.1.0.md
#>
# SPDX-License-Identifier: MIT OR Apache-2.0
param(
    [string]$Repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path,
    [string]$Ref = 'main',
    [string]$Project = $(if ($env:CI_PROJECT_PATH) { $env:CI_PROJECT_PATH } else { 'cino-group/udm' }),
    [string]$GitLab = $(if ($env:CI_SERVER_URL) { $env:CI_SERVER_URL } else { 'https://gitlab.com' }),
    [string]$NotesFile,
    [string]$PfxPath,
    [switch]$AllowUnsigned,
    [switch]$SkipTests,
    [switch]$Prerelease,
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2

function Step($text) { Write-Host "`n== $text" -ForegroundColor Cyan }
function Quiet { $old = $ErrorActionPreference; $ErrorActionPreference = 'Continue'; try { & $args[0] @($args | Select-Object -Skip 1) 2>&1 | Out-Null } finally { $ErrorActionPreference = $old } }
function Run($exe) {
    $old = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
    try { & $exe @args } finally { $ErrorActionPreference = $old }
    if ($LASTEXITCODE -ne 0) { throw "$exe failed with exit code $LASTEXITCODE" }
}
function Unprotect($secure) {
    [Runtime.InteropServices.Marshal]::PtrToStringBSTR([Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure))
}

$projectId = [Uri]::EscapeDataString($Project)
$api = "$GitLab/api/v4/projects/$projectId"
if ($NotesFile) { $NotesFile = (Resolve-Path $NotesFile).Path }

# ---------------------------------------------------------------- token
$token = $env:GITLAB_TOKEN
# saved once with: Read-Host -AsSecureString | Export-Clixml "$env:LOCALAPPDATA\UDM-release\gitlab-token.clixml"
$tokenFile = Join-Path $env:LOCALAPPDATA 'UDM-release\gitlab-token.clixml'
if (-not $token -and (Test-Path $tokenFile)) { $token = Unprotect (Import-Clixml $tokenFile) }
$headers = @{ 'PRIVATE-TOKEN' = $token }
if (-not $token -and $env:CI_JOB_TOKEN) {
    $headers = @{ 'JOB-TOKEN' = $env:CI_JOB_TOKEN }
} elseif (-not $token -and -not $DryRun) {
    $token = Unprotect (Read-Host 'GitLab access token (scope: api)' -AsSecureString)
    $headers = @{ 'PRIVATE-TOKEN' = $token }
}

# ---------------------------------------------------------------- source checks
Step "Checking $Ref"
$commit = (git -C $Repo rev-parse --verify "$Ref^{commit}").Trim()
if ($LASTEXITCODE -ne 0) { throw "Unknown ref $Ref" }
# the same net as tools\publish-gitlab-snapshot.ps1 uses over the public tree
$private = '(?i)(^|/)(AGENTS?\.md|AGENT_START_HERE\.md|note_for_agents?\.md|dev-log\.md|PRIVATE_DOCS[^/]*)$|^review-artifacts/|(^|/)\.env(\.|$)|\.(pfx|p12|pem|key|redb)$|(^|/)\.signing/'
$leak = @(git -C $Repo ls-tree -r --name-only $commit | Where-Object { $_ -match $private })
if ($leak.Count) {
    throw "$Ref contains private documents ($($leak[0]), ...). Release from the GitLab clone, after tools\publish-gitlab-snapshot.ps1."
}
if (-not $env:CI) {
    # a release must point at public source: the commit has to be on GitLab main already
    Quiet git -C $Repo fetch --quiet origin main
    Quiet git -C $Repo merge-base --is-ancestor $commit origin/main
    if ($LASTEXITCODE -ne 0 -and -not $DryRun) { throw "$commit is not on GitLab main (origin/main). Push the snapshot first." }
}

# ---------------------------------------------------------------- worktree
Step "Preparing a clean copy of $Ref"
# inside the ignored target folder, not %TEMP%: antivirus locks fresh build scripts there
$work = Join-Path $Repo "target\release-gitlab\src-$($commit.Substring(0, 8))"
if (Test-Path $work) {
    Quiet git -C $Repo worktree remove --force $work
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
Run git -C $Repo worktree add --detach $work $commit

try {
    Set-Location $work
    $cargo = Get-Content Cargo.toml -Raw
    if ($cargo -notmatch '(?ms)^\[workspace\.package\].*?^version\s*=\s*"([^"]+)"') { throw 'No workspace version in Cargo.toml' }
    $version = $Matches[1]
    $tag = "v$version"
    Write-Host "Version $version, tag $tag, commit $commit"
    if ($env:CI_COMMIT_TAG -and $env:CI_COMMIT_TAG -ne $tag) {
        throw "Tag $($env:CI_COMMIT_TAG) does not match the Cargo.toml version $version"
    }
    if ($version.Contains('-') -and -not $Prerelease) {
        # GitLab has no pre-release flag: the newest release becomes "latest" for every download link
        throw "$version is a pre-release. Pass -Prerelease if it really should become the latest GitLab release."
    }
    if (-not $DryRun) {
        $exists = $true
        try { Invoke-RestMethod -Headers $headers "$api/releases/$tag" | Out-Null }
        catch { $exists = $false }
        if ($exists) { throw "GitLab release $tag already exists. Delete it first or raise the version." }
    }

    # ------------------------------------------------------------ notes (before the long build)
    function Get-Section($file) {
        if (-not (Test-Path $file)) { return $null }
        $section = @(); $found = $false
        foreach ($line in Get-Content $file -Encoding utf8) {
            if ($line.StartsWith('## ')) {
                if ($found) { break }
                $found = $line -match "^## \[?v?$([regex]::Escape($version))\]?(\s|$)"
            } elseif ($found) { $section += $line }
        }
        if ($found) { return ($section -join "`n").Trim() }
        return $null
    }
    if ($NotesFile) {
        $notes = (Get-Content $NotesFile -Raw -Encoding utf8).Trim()
    } else {
        $notes = Get-Section CHANGELOG.md
        if (-not $notes) {
            if (-not $DryRun) { throw "No release notes: pass -NotesFile, or add a '## $version' section to CHANGELOG.md." }
            $notes = "(dry run: no CHANGELOG.md section for $version)"
        }
        $vi = Get-Section CHANGELOG.vi.md
        if ($vi) { $notes += "`n`n## Ti$([char]0x1EBF)ng Vi$([char]0x1EC7)t`n`n$vi" }
    }

    # ------------------------------------------------------------ signing key (before the long build)
    $env:CODESIGN_PROJECT_NAME = 'UDM'
    $cer = Join-Path $work 'docs\signing\UDM-codesign.cer'
    $buildArgs = @('-NoSlintCompile')
    $thumb = $null
    if ($AllowUnsigned) {
        Write-Host 'UNSIGNED release (-AllowUnsigned): only with the owner''s approval for this release' -ForegroundColor Yellow
    } else {
        if (-not (Test-Path $cer)) { throw "$Ref has no docs\signing\UDM-codesign.cer, so it cannot be signed. Publish a snapshot that has it." }
        $thumb = (New-Object Security.Cryptography.X509Certificates.X509Certificate2 $cer).Thumbprint.ToUpperInvariant()
        $buildArgs += '-Sign'
        if (Test-Path "Cert:\CurrentUser\My\$thumb") {
            Write-Host "Signing with certificate $thumb in CurrentUser\My"
        } else {
            $localPfx = Join-Path $Repo '.signing\UDM-codesign.pfx'
            if (-not $PfxPath -and (Test-Path $localPfx)) { $PfxPath = $localPfx }
            if (-not $PfxPath) { throw "No signing key: certificate $thumb is not in CurrentUser\My and no PFX was given (-PfxPath). Pass -AllowUnsigned only with the owner's approval." }
            if (-not $env:UDM_SIGN_PFX_PASSWORD) {
                $pwFile = Join-Path $env:LOCALAPPDATA 'UDM-signing\pfx-password.clixml'
                $securePw = if (Test-Path $pwFile) { Import-Clixml $pwFile } else { Read-Host 'PFX password' -AsSecureString }
                $env:UDM_SIGN_PFX_PASSWORD = Unprotect $securePw
            }
            $PfxPath = (Resolve-Path $PfxPath).Path
            $buildArgs += @('-PfxPath', $PfxPath)
            Write-Host "Signing with $PfxPath (pinned $thumb)"
        }
    }

    # ------------------------------------------------------------ test and build
    if (-not (Get-Command makensis -ErrorAction SilentlyContinue)) {
        $nsis = @("${env:ProgramFiles(x86)}\NSIS", "$env:ProgramFiles\NSIS") | Where-Object { Test-Path "$_\makensis.exe" } | Select-Object -First 1
        if (-not $nsis) { throw 'NSIS 3 not found. Run "winget install NSIS.NSIS" and try again.' }
        $env:Path = "$nsis;$env:Path"
    }
    if (-not $SkipTests) {
        Step 'Testing'
        Run cargo test --workspace --locked --no-default-features
    }
    Step 'Building and packaging (packaging\windows\build.ps1)'
    # in its own process: build.ps1 runs cargo under 'Stop', and Windows PowerShell turns cargo's
    # stderr progress into errors when the output is captured (as in CI)
    try {
        Run powershell -NoProfile -ExecutionPolicy Bypass -File .\packaging\windows\build.ps1 @buildArgs
    } finally {
        Remove-Item Env:\UDM_SIGN_PFX_PASSWORD -ErrorAction SilentlyContinue
    }
    $dist = Join-Path $work 'dist'
    $setup = "udm-$version-setup.exe"
    $zip = "udm-$version-portable.zip"
    foreach ($f in @($setup, $zip)) {
        if (-not (Test-Path (Join-Path $dist $f))) { throw "build.ps1 did not produce $f" }
    }

    $out = Join-Path $work 'release-out'
    New-Item -ItemType Directory -Force $out | Out-Null
    Copy-Item (Join-Path $dist $setup), (Join-Path $dist $zip), LICENSE $out
    if ($thumb) {
        Step 'Checking signatures against the pinned certificate'
        $verify = Join-Path $work 'tools\signing\verify.ps1'
        & $verify -Path (Join-Path $dist 'udm.exe') -ExpectedThumbprint $thumb -ProjectName UDM -JsonPath (Join-Path $out 'udm-signature.json')
        & $verify -Path (Join-Path $dist 'udm-cli.exe') -ExpectedThumbprint $thumb -ProjectName UDM
        & $verify -Path (Join-Path $out $setup) -ExpectedThumbprint $thumb -ProjectName UDM -JsonPath (Join-Path $out "udm-$version-setup-signature.json")
        # the portable zip must carry the signed executables, not a stale copy
        $check = Join-Path $work 'zip-check'
        Expand-Archive (Join-Path $out $zip) $check -Force
        foreach ($exe in Get-ChildItem $check -Recurse -Filter *.exe) {
            & $verify -Path $exe.FullName -ExpectedThumbprint $thumb -ProjectName UDM
        }
        Copy-Item $cer $out
    }
    $lines = foreach ($f in @($setup, $zip)) {
        "$((Get-FileHash (Join-Path $out $f) -Algorithm SHA256).Hash.ToLower())  $f"
    }
    [IO.File]::WriteAllText("$out\SHA256SUMS.txt", ($lines -join "`n") + "`n")
    $lines | ForEach-Object { Write-Host "  $_" }

    $blob = "$GitLab/$Project/-/blob/$tag"
    $notes += "`n`n---`n`nRun ``$setup`` to install for the current user (no administrator rights needed), " +
        "or unzip ``$zip`` and run ``udm.exe`` (portable: data stays next to the program). Windows 11 x64.`n`n" +
        $(if ($thumb) {
            "Signed with the self-signed UDM code signing certificate, thumbprint ``$thumb`` (``UDM-codesign.cer`` in this release). " +
            "Windows does not know this certificate, so SmartScreen may still warn the first time. "
        } else {
            "**This release is not code signed.** Windows SmartScreen may warn the first time. "
        }) +
        "Check the files against ``SHA256SUMS.txt``. The browser extension host is not packaged yet; " +
        "see [docs/BUILD.md]($blob/docs/BUILD.md).`n"
    [IO.File]::WriteAllText("$out\release-notes.md", $notes, (New-Object Text.UTF8Encoding $false))

    $files = @(Get-ChildItem $out -File | Where-Object Name -ne 'release-notes.md' | ForEach-Object Name)
    if ($DryRun) {
        Step "Dry run: nothing uploaded"
        $files + 'release-notes.md' | ForEach-Object { Write-Host "  $_" }
        $keep = Join-Path $Repo "target\release-gitlab\out-$version"
        Remove-Item -Recurse -Force $keep -ErrorAction SilentlyContinue
        Move-Item $out $keep
        Write-Host "Kept in $keep"
        return
    }

    # ------------------------------------------------------------ upload
    Step 'Uploading to the GitLab package registry'
    # stable paths, so .../-/releases/permalink/latest/downloads/<path> always gets the newest file
    $stable = @{ $setup = 'udm-setup.exe'; $zip = 'udm-portable.zip' }
    $links = @()
    foreach ($f in $files) {
        $url = "$api/packages/generic/udm/$version/$f"
        Invoke-RestMethod -Method Put -Headers $headers -InFile (Join-Path $out $f) -Uri $url | Out-Null
        Write-Host "  $f"
        $path = if ($stable.ContainsKey($f)) { $stable[$f] } else { $f }
        $links += @{ name = $f; url = $url; direct_asset_path = "/$path"; link_type = $(if ($stable.ContainsKey($f)) { 'package' } else { 'other' }) }
    }

    Step "Creating release $tag"
    $body = @{
        name = "UDM $version"
        tag_name = $tag
        ref = $commit
        description = $notes
        assets = @{ links = $links }
    } | ConvertTo-Json -Depth 5
    Invoke-RestMethod -Method Post -Headers $headers -Uri "$api/releases" `
        -ContentType 'application/json; charset=utf-8' -Body ([Text.Encoding]::UTF8.GetBytes($body)) | Out-Null
    Write-Host "Done: $GitLab/$Project/-/releases/$tag"
    Write-Host "Latest installer: $GitLab/$Project/-/releases/permalink/latest/downloads/udm-setup.exe"
}
finally {
    Set-Location $Repo
    Quiet git -C $Repo worktree remove --force $work
}
