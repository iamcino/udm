<#
.SYNOPSIS
  Publishes a sanitized snapshot of the private UDM repository on GitLab (cino-group/udm).

.DESCRIPTION
  GitLab does not share history with the private GitHub repository. Each snapshot is one new
  commit on GitLab main whose tree is the tree of -Ref minus the private documents, with the
  previous GitLab main as its only parent. No private commit, blob or message is ever reachable
  from what is pushed.

  Removed from every snapshot:
    - AGENTS.md, AGENT_START_HERE.md, dev-log.md, note_for_agents.md,
      PRIVATE_DOCS_REMOVE_BEFORE_PUBLIC.md and the whole review-artifacts/ folder
    - every path listed in PRIVATE_DOCS_REMOVE_BEFORE_PUBLIC.md at -Ref
  The script then checks the new tree again and stops if anything private or secret-looking is
  left.

  The commit is built in this repository with a throwaway index (your checkout and index are
  not touched), handed to the GitLab working copy (-Snapshot, a clone of cino-group/udm) and
  fast-forwarded there. Nothing is pushed unless you pass -Push.

.EXAMPLE
  .\tools\publish-gitlab-snapshot.ps1               # build the snapshot of main, show it, do not push
.EXAMPLE
  .\tools\publish-gitlab-snapshot.ps1 -Push         # same, then push GitLab main
.EXAMPLE
  .\tools\publish-gitlab-snapshot.ps1 -Ref v0.2.0 -Message 'UDM 0.2.0' -Push
#>
# SPDX-License-Identifier: MIT OR Apache-2.0
param(
    [string]$Repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path,
    [string]$Ref = 'main',
    # a clone of git@gitlab.com:cino-group/udm.git, used only to push
    [string]$Snapshot = 'C:\work\udm-gitlab',
    # the fetch-only remote in -Repo that points at GitLab
    [string]$GitLabRemote = 'gitlab',
    [string]$Message,
    [switch]$Push
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2

function Step($text) { Write-Host "`n== $text" -ForegroundColor Cyan }
function RunGit {
    $out = & git.exe @args
    if ($LASTEXITCODE -ne 0) { throw "git $($args -join ' ') failed with exit code $LASTEXITCODE" }
    $out
}

# always private, even if the manifest is gone or forgets them
$fixed = @('AGENTS.md', 'AGENT_START_HERE.md', 'dev-log.md', 'note_for_agents.md',
    'PRIVATE_DOCS_REMOVE_BEFORE_PUBLIC.md', 'review-artifacts')
# a second net over the final tree: names that must never reach GitLab
$forbidden = '(?i)(^|/)(AGENTS?\.md|AGENT_START_HERE\.md|note_for_agents?\.md|dev-log\.md|PRIVATE_DOCS[^/]*)$|^review-artifacts/|(^|/)\.env(\.|$)|\.(pfx|p12|pem|key|redb)$|(^|/)\.signing/'

# ---------------------------------------------------------------- source
Step "Source $Ref"
$commit = (RunGit -C $Repo rev-parse --verify "$Ref^{commit}").Trim()
Write-Host "commit $commit  $((RunGit -C $Repo log -1 --format=%s $commit))"

$manifest = @()
if (RunGit -C $Repo ls-tree --name-only $commit -- PRIVATE_DOCS_REMOVE_BEFORE_PUBLIC.md) {
    # the paths are ASCII, so the console code page does not matter here
    $text = RunGit -C $Repo show "${commit}:PRIVATE_DOCS_REMOVE_BEFORE_PUBLIC.md"
    foreach ($line in $text) {
        if ($line -match '^\s*-\s+`([^`]+)`\s*$') { $manifest += $Matches[1] }
    }
    Write-Host "manifest lists $($manifest.Count) private paths"
} else {
    Write-Host 'no PRIVATE_DOCS_REMOVE_BEFORE_PUBLIC.md at this ref; using the fixed list only'
}
$private = @($fixed + $manifest | Sort-Object -Unique)

# ---------------------------------------------------------------- GitLab base
Step 'GitLab main'
RunGit -C $Repo fetch --quiet $GitLabRemote main | Out-Null
$base = (RunGit -C $Repo rev-parse --verify "refs/remotes/$GitLabRemote/main^{commit}").Trim()
Write-Host "GitLab main is $base  $((RunGit -C $Repo log -1 --format=%s $base))"

if (-not (Test-Path (Join-Path $Snapshot '.git'))) { throw "$Snapshot is not a clone of cino-group/udm" }
if (RunGit -C $Snapshot status --porcelain) { throw "$Snapshot has uncommitted changes" }
RunGit -C $Snapshot fetch --quiet origin main | Out-Null
$snapRemote = (RunGit -C $Snapshot rev-parse origin/main).Trim()
if ($snapRemote -ne $base) { throw "$Snapshot sees GitLab main at $snapRemote, but $GitLabRemote/main is $base. Fetch both and retry." }
if ((RunGit -C $Snapshot rev-parse --abbrev-ref HEAD).Trim() -ne 'main') { throw "$Snapshot is not on main" }
RunGit -C $Snapshot merge --quiet --ff-only origin/main | Out-Null

# ---------------------------------------------------------------- tree
Step 'Building the public tree'
$index = Join-Path ([IO.Path]::GetTempPath()) "udm-snapshot-$PID.index"
$oldIndex = $env:GIT_INDEX_FILE
try {
    $env:GIT_INDEX_FILE = $index
    RunGit -C $Repo read-tree $commit | Out-Null
    foreach ($p in $private) {
        $gone = & git -C $Repo rm --cached -r -q --ignore-unmatch -- $p
        if ($LASTEXITCODE -ne 0) { throw "could not drop $p" }
    }
    $tree = (RunGit -C $Repo write-tree).Trim()
} finally {
    if ($null -eq $oldIndex) { Remove-Item Env:\GIT_INDEX_FILE -ErrorAction SilentlyContinue } else { $env:GIT_INDEX_FILE = $oldIndex }
    Remove-Item $index -Force -ErrorAction SilentlyContinue
}

$files = @(RunGit -C $Repo ls-tree -r --name-only $tree)
$leak = @($files | Where-Object { $_ -match $forbidden })
foreach ($p in $private) {
    $leak += @($files | Where-Object { $_ -eq $p -or $_.StartsWith("$p/") })
}
if ($leak.Count) { throw "Private files are still in the public tree:`n  $(($leak | Sort-Object -Unique) -join "`n  ")" }
Write-Host "$($files.Count) files, none private"

if ($tree -eq (RunGit -C $Repo rev-parse "$base^{tree}").Trim()) {
    Write-Host "`nGitLab main already has exactly this tree. Nothing to publish." -ForegroundColor Yellow
    return
}

# ---------------------------------------------------------------- commit
Step 'Snapshot commit'
$short = $commit.Substring(0, 7)
$subject = if ($Message) { $Message } else { 'UDM public snapshot update' }
$body = "Source: iamcino/udm @ $short. Private agent documents are not included."
# same identity as the earlier snapshots, taken from the GitLab working copy
$env:GIT_AUTHOR_NAME = $env:GIT_COMMITTER_NAME = (RunGit -C $Snapshot config user.name).Trim()
$env:GIT_AUTHOR_EMAIL = $env:GIT_COMMITTER_EMAIL = (RunGit -C $Snapshot config user.email).Trim()
# through a file: piping text to git from Windows PowerShell adds a byte order mark
$msgFile = Join-Path ([IO.Path]::GetTempPath()) "udm-snapshot-$PID.msg"
[IO.File]::WriteAllText($msgFile, "$subject`n`n$body`n", (New-Object Text.UTF8Encoding $false))
try {
    $new = (RunGit -C $Repo commit-tree $tree -p $base -F $msgFile).Trim()
} finally {
    Remove-Item $msgFile -Force -ErrorAction SilentlyContinue
    Remove-Item Env:\GIT_AUTHOR_NAME, Env:\GIT_COMMITTER_NAME, Env:\GIT_AUTHOR_EMAIL, Env:\GIT_COMMITTER_EMAIL -ErrorAction SilentlyContinue
}
RunGit -C $Repo diff --stat $base $new | Write-Host

# hand only this commit (and what it reaches: GitLab history + the clean tree) to the GitLab copy
$pending = 'refs/gitlab-snapshot/pending'
RunGit -C $Repo update-ref $pending $new | Out-Null
try {
    RunGit -C $Snapshot fetch --quiet --no-tags $Repo $pending | Out-Null
    RunGit -C $Snapshot merge --quiet --ff-only $new | Out-Null
} finally {
    RunGit -C $Repo update-ref -d $pending | Out-Null
}
Write-Host "$Snapshot main is now $new"

if ($Push) {
    Step 'Pushing GitLab main'
    RunGit -C $Snapshot push origin main
    Write-Host "Done: https://gitlab.com/cino-group/udm/-/commit/$new"
} else {
    Write-Host "`nNot pushed. Check it, then run:  git -C $Snapshot push origin main" -ForegroundColor Yellow
}
