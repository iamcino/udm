param(
    [Parameter(Mandatory=$true)][ValidateSet('setup','run','recover')][string]$Action,
    [Parameter(Mandatory=$true)][string]$TrialRoot,
    [Parameter(Mandatory=$true)][string]$TestExe,
    [ValidateRange(1,16384)][int]$MiB = 1,
    [ValidateSet('Prepared','Renamed','MetadataSynced','BeforeCompletion','CompletionRecorded')]
    [string]$Cut = 'Prepared',
    [ValidateSet('exit','hold')][string]$Mode = 'exit'
)
$ErrorActionPreference = 'Stop'
# Explicit binary avoids picking a stale test executable from a wildcard.
$actor = (Resolve-Path -LiteralPath $TestExe).Path
$trial = [System.IO.Path]::GetFullPath($TrialRoot)
if (-not [System.IO.Path]::IsPathRooted($TrialRoot)) { throw 'Use an absolute trial root.' }
if ($Action -eq 'setup' -and (Test-Path -LiteralPath $trial)) {
    throw 'Setup refuses an existing path. Choose a new isolated trial root.'
}
if ($Action -ne 'setup' -and -not (Test-Path -LiteralPath (Join-Path $trial 'manifest.json'))) {
    throw 'Trial manifest missing. Do not run against daily-use UDM data.'
}
Write-Output "actor_sha256=$((Get-FileHash -LiteralPath $actor -Algorithm SHA256).Hash)"
Write-Output "action=$Action trial=$trial cut=$Cut mode=$Mode"
if ($Action -eq 'run' -and $Mode -eq 'hold') {
    Write-Output 'HOLD does not power off anything. Wait for a UDM_CAMPAIGN event with held=true.'
    Write-Output 'Only the confirmed disposable VM may be powered off by its host operator.'
}
$start = [System.Diagnostics.ProcessStartInfo]::new()
$start.FileName = $actor
$start.Arguments = '--exact finalize::campaign_tests::campaign_actor --ignored --nocapture'
$start.UseShellExecute = $false
$start.CreateNoWindow = $true
$start.RedirectStandardOutput = $true
$start.RedirectStandardError = $true
$start.Environment['UDM_CAMPAIGN_ACTION'] = $Action
$start.Environment['UDM_CAMPAIGN_ROOT'] = $trial
$start.Environment['UDM_CAMPAIGN_MIB'] = "$MiB"
$start.Environment['UDM_CAMPAIGN_CUT'] = $Cut
$start.Environment['UDM_CAMPAIGN_MODE'] = $Mode
# Relay to the caller/host. Hidden child processes do not reliably inherit
# PowerShell's output handles on Windows, so explicitly forward each line.
$process = [System.Diagnostics.Process]::Start($start)
$stderr = $process.StandardError.ReadToEndAsync()
while ($null -ne ($line = $process.StandardOutput.ReadLine())) { Write-Output $line }
$process.WaitForExit()
$errorText = $stderr.GetAwaiter().GetResult()
if ($errorText) { Write-Output $errorText }
$code = $process.ExitCode
$process.Dispose()
if ($Action -eq 'run' -and $Mode -eq 'exit') {
    if ($code -ne 86) { throw "Expected abrupt-exit code 86; got $code" }
    Write-Output 'PROCESS CUT ONLY: exit 86 observed; not a VM power-loss result.'
} elseif ($code -ne 0) {
    throw "Campaign actor failed: exit $code"
}
