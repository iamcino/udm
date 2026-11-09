<#
.SYNOPSIS
  Securely loads CI variables (GitLab: Settings > CI/CD > Variables, protected + masked) onto the clipboard for easy one-time pasting.

.DESCRIPTION
  1. Base64 PFX: CODESIGN_PFX_BASE64
  2. PFX Password: CODESIGN_PFX_PASSWORD
  3. Pinned Thumbprint: CODESIGN_EXPECTED_THUMBPRINT
  Clears clipboard automatically upon completion.
#>
# SPDX-License-Identifier: MIT OR Apache-2.0
param([string]$ProjectName)

. (Join-Path $PSScriptRoot 'common.ps1')

if ($ProjectName) {
    $script:ProjectName = $ProjectName
    $script:LocalPfxPath = Join-Path $script:SigningDir "$script:ProjectName-codesign.pfx"
    $script:PasswordPath = Join-Path $env:LOCALAPPDATA "$script:ProjectName-signing\pfx-password.clixml"
}

if (-not (Test-Path $script:LocalPfxPath)) {
    throw "PFX not found: $script:LocalPfxPath. Run new-cert.ps1 first."
}
if (-not (Test-Path $script:PasswordPath)) {
    throw "DPAPI password file not found: $script:PasswordPath"
}

$thumbprint = Get-PinnedThumbprint
$upperName = $script:ProjectName.ToUpper()

Write-Host "`n=== CI VARIABLES SETUP ===" -ForegroundColor Cyan
Write-Host "Go to: GitLab project -> Settings -> CI/CD -> Variables (protected, masked)`n"

try {
    # 1. Base64 PFX
    $pfxBase64 = [Convert]::ToBase64String([IO.File]::ReadAllBytes($script:LocalPfxPath))
    Set-Clipboard -Value $pfxBase64
    Write-Host "[1/2] Variable: ${upperName}_SIGN_PFX_BASE64" -ForegroundColor Yellow
    Write-Host "      Base64 PFX has been copied to clipboard!" -ForegroundColor Green
    Read-Host "      Paste into the CI variable, then press Enter to continue"

    # 2. Password
    $secure = Import-Clixml -Path $script:PasswordPath
    $plainPassword = [System.Net.NetworkCredential]::new('', $secure).Password
    Set-Clipboard -Value $plainPassword
    Write-Host "[2/2] Variable: ${upperName}_SIGN_PFX_PASSWORD" -ForegroundColor Yellow
    Write-Host "      PFX password has been copied to clipboard!" -ForegroundColor Green
    Read-Host "      Paste into the CI variable, then press Enter to continue"
} finally {
    Set-Clipboard -Value ' '
    Write-Host "`nClipboard cleared." -ForegroundColor Green
}

Write-Host "`nVariable: CODESIGN_EXPECTED_THUMBPRINT (or ${upperName}_EXPECTED_THUMBPRINT)" -ForegroundColor Yellow
Write-Host "Value: $thumbprint" -ForegroundColor Cyan
Write-Host "(This thumbprint is not secret, add as a repository variable)`n"
