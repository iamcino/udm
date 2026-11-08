param(
    [Parameter(Mandatory=$true)][string]$TestExe,
    [Parameter(Mandatory=$true)][string]$ArtifactRoot,
    [ValidateRange(1,16384)][int[]]$SizesMiB = @(4096,16384),
    [ValidateRange(1,20)][int]$Samples = 5,
    [switch]$ReclaimPassedPayload
)
$ErrorActionPreference = 'Stop'
$actor = (Resolve-Path -LiteralPath $TestExe).Path
if (-not [IO.Path]::IsPathRooted($ArtifactRoot)) { throw 'ArtifactRoot must be absolute.' }
$campaignRoot = [IO.Path]::GetFullPath($ArtifactRoot).TrimEnd('\')
if (Test-Path -LiteralPath $campaignRoot) { throw 'Choose a fresh artifact root; trials are never overwritten.' }
New-Item -ItemType Directory -Path $campaignRoot | Out-Null
$actorHash = (Get-FileHash -LiteralPath $actor -Algorithm SHA256).Hash
$actorHash | Set-Content -LiteralPath (Join-Path $campaignRoot 'actor-sha256.txt')
@{
    actor=$actor; sha256=$actorHash; os=[Environment]::OSVersion.VersionString
    powershell=$PSVersionTable.PSVersion.ToString(); utc_started=[DateTime]::UtcNow.ToString('o')
    sizes_mib=$SizesMiB; samples=$Samples; reclaim_passed=[bool]$ReclaimPassedPayload
    note='Cached synthetic finalize/recovery measurements; no VM power-off or disk-read counters.'
} | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $campaignRoot 'environment.json')

function Invoke-CampaignActor([string]$Action, [string]$Trial, [int]$MiB, [string]$LogStem) {
    $start = [Diagnostics.ProcessStartInfo]::new($actor)
    $start.Arguments = '--exact finalize::campaign_tests::campaign_actor --ignored --nocapture'
    $start.UseShellExecute=$false
    $start.CreateNoWindow=$true
    $start.RedirectStandardOutput=$true
    $start.RedirectStandardError=$true
    $start.Environment['UDM_CAMPAIGN_ACTION']=$Action
    $start.Environment['UDM_CAMPAIGN_ROOT']=$Trial
    $start.Environment['UDM_CAMPAIGN_MIB']="$MiB"
    $start.Environment['UDM_CAMPAIGN_CUT']='Renamed'
    $start.Environment['UDM_CAMPAIGN_MODE']='exit'
    $lines=[Collections.Generic.List[string]]::new()
    $events=[Collections.Generic.List[object]]::new()
    $watch=[Diagnostics.Stopwatch]::StartNew()
    $process=[Diagnostics.Process]::Start($start)
    $stderr=$process.StandardError.ReadToEndAsync()
    $peak=0L
    $cpuMs=0.0
    $metricSamples=0
    try {
        while ($true) {
            $read=$process.StandardOutput.ReadLineAsync()
            do {
                $ready=$read.Wait(100)
                try {
                    $process.Refresh()
                    $peak=[Math]::Max($peak,$process.WorkingSet64)
                    $cpuMs=$process.TotalProcessorTime.TotalMilliseconds
                    $metricSamples++
                } catch { } # Very short children may exit before a sample.
                if ($watch.Elapsed.TotalMinutes -gt 20) { throw 'Actor timed out; preserve trial for diagnosis.' }
            } while (-not $ready)
            $line=$read.Result
            if ($null -eq $line) { break }
            $lines.Add($line)
            # libtest may prefix the first marker with the test name.
            $marker=$line.IndexOf('UDM_CAMPAIGN ',[StringComparison]::Ordinal)
            if ($marker -ge 0) { $events.Add(($line.Substring($marker+13) | ConvertFrom-Json)) }
        }
        $process.WaitForExit()
        $exitCode=$process.ExitCode
        try { $cpuMs=$process.TotalProcessorTime.TotalMilliseconds } catch { }
        $watch.Stop()
        $lines | Set-Content -LiteralPath "$LogStem.stdout.log"
        $stderr.GetAwaiter().GetResult() | Set-Content -LiteralPath "$LogStem.stderr.log"
        $expected=if ($Action -eq 'run') {86} else {0}
        if ($exitCode -ne $expected) { throw "$Action exit $exitCode, expected $expected; see $LogStem" }
        return [pscustomobject]@{
            exit_code=$exitCode; wall_ms=$watch.Elapsed.TotalMilliseconds
            cpu_ms=$cpuMs; sampled_peak_working_set_bytes=$peak; metric_samples=$metricSamples
            events=@($events.ToArray())
        }
    } finally {
        if (-not $process.HasExited) { $process.Kill(); $process.WaitForExit() }
        $lines | Set-Content -LiteralPath "$LogStem.stdout.log"
        $process.Dispose()
    }
}

$results=[Collections.Generic.List[object]]::new()
foreach ($mib in $SizesMiB) {
    for ($sample=1; $sample -le $Samples; $sample++) {
        # Leave a 2 GiB margin above incoming bytes. Setup uses a small old target.
        $drive=[IO.DriveInfo]::new([IO.Path]::GetPathRoot($campaignRoot))
        if ($drive.AvailableFreeSpace -lt (($mib * 1MB) + 2GB)) { throw 'Insufficient free space; completed raw samples retained.' }
        $name="mib-$mib-sample-$sample"
        $trial=Join-Path $campaignRoot $name
        Write-Output "START $name"
        $setup=Invoke-CampaignActor setup $trial $mib (Join-Path $campaignRoot "$name-setup")
        $run=Invoke-CampaignActor run $trial $mib (Join-Path $campaignRoot "$name-run")
        $pending=Invoke-CampaignActor recover $trial $mib (Join-Path $campaignRoot "$name-pending")
        $committed=Invoke-CampaignActor recover $trial $mib (Join-Path $campaignRoot "$name-committed")
        $manifest=Get-Content -LiteralPath (Join-Path $trial 'manifest.json') -Raw | ConvertFrom-Json
        $payload=Join-Path $trial 'payload.bin'
        $info=Get-Item -LiteralPath $payload
        if ($info.Length -ne $manifest.bytes) { throw 'Independent length oracle failed.' }
        $actual=(Get-FileHash -LiteralPath $payload -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actual -ne $manifest.new_sha256) { throw 'Independent SHA256 oracle failed; payload retained.' }
        $pendingEvent=@($pending.events | Where-Object event -eq 'RecoveryVerified')
        $committedEvent=@($committed.events | Where-Object event -eq 'RecoveryVerified')
        $hashEvent=@($run.events | Where-Object event -eq 'IntegrityVerified')
        if ($pendingEvent.Count -ne 1 -or $committedEvent.Count -ne 1 -or $hashEvent.Count -ne 1) {
            throw 'Missing or duplicate phase event; raw logs retained.'
        }
        $row=[pscustomobject]@{
            size_mib=$mib; sample=$sample; sha256=$actual; bytes=$info.Length
            pre_finalize_check_us=$hashEvent[0].elapsed_us
            pending_recovery_us=$pendingEvent[0].recovery_us
            committed_recovery_us=$committedEvent[0].recovery_us
            setup=$setup; run=$run; pending=$pending; committed=$committed
            independent_sha256_pass=$true; payload_reclaimed=$false
        }
        $results.Add($row)
        # Save evidence BEFORE deleting only a known generated payload. The
        # retained Store will intentionally reference a removed artifact after cleanup.
        $results.ToArray() | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath (Join-Path $campaignRoot 'samples.json')
        if ($ReclaimPassedPayload) {
            $resolved=(Resolve-Path -LiteralPath $payload).Path
            if (-not $resolved.StartsWith($campaignRoot+'\',[StringComparison]::OrdinalIgnoreCase) -or
                $info.Name -ne 'payload.bin' -or ($info.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
                throw 'Refuse cleanup outside campaign or through a reparse-point payload.'
            }
            Remove-Item -LiteralPath $resolved
            $row.payload_reclaimed=$true
            $results.ToArray() | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath (Join-Path $campaignRoot 'samples.json')
        }
        Write-Output "PASS $name pending_us=$($row.pending_recovery_us) committed_us=$($row.committed_recovery_us) payload_reclaimed=$($row.payload_reclaimed)"
    }
}
Write-Output "COMPLETE samples=$($results.Count) artifact_root=$campaignRoot"
