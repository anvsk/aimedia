[CmdletBinding()]
param(
    [string]$EngineImage = "aimedia:gpu",
    [string]$PeerImage = "aimedia:test-tools",
    [ValidateRange(60, 86400)]
    [int]$DurationSeconds = 7200,
    [ValidateRange(5, 60)]
    [int]$SampleIntervalSeconds = 30,
    [ValidateRange(0, 1800)]
    [int]$WarmupSeconds = 60,
    [switch]$KeepContainers
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$runId = [Guid]::NewGuid().ToString("N").Substring(0, 8)
$network = "am-soak-$runId"
$source = "am-$runId-source"
$probe = "am-$runId-probe"
$engine = "am-$runId-engine"
$runRoot = Join-Path ([IO.Path]::GetTempPath()) "aimedia-soak-$runId"
$configPath = Join-Path $runRoot "job.yaml"
$samplesPath = Join-Path $runRoot "samples.jsonl"
$containers = @($source, $probe, $engine)
$samples = [Collections.Generic.List[object]]::new()

function Invoke-Docker {
    param(
        [Parameter(Mandatory)][string[]]$Arguments,
        [switch]$Capture
    )

    if ($Capture) {
        $output = & docker @Arguments 2>&1
        if ($LASTEXITCODE -ne 0) {
            throw "docker $($Arguments -join ' ') failed:`n$($output -join [Environment]::NewLine)"
        }
        return $output
    }
    & docker @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "docker $($Arguments -join ' ') failed with exit code $LASTEXITCODE"
    }
}

function Start-Container {
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][string[]]$Arguments
    )
    $null = Invoke-Docker -Arguments (@("run", "-d", "--name", $Name) + $Arguments) -Capture
}

function Wait-ContainerExit {
    param(
        [Parameter(Mandatory)][string]$Name,
        [int]$TimeoutSeconds = 30,
        [switch]$AllowFailure
    )
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        $running = (Invoke-Docker -Arguments @("inspect", "-f", "{{.State.Running}}", $Name) -Capture).Trim()
        if ($running -ne "true") {
            $exitCode = [int](Invoke-Docker -Arguments @("inspect", "-f", "{{.State.ExitCode}}", $Name) -Capture)
            if (!$AllowFailure -and $exitCode -ne 0) {
                $logs = Invoke-Docker -Arguments @("logs", "--tail", "120", $Name) -Capture
                throw "$Name exited with ${exitCode}:`n$($logs -join [Environment]::NewLine)"
            }
            return $exitCode
        }
        Start-Sleep -Milliseconds 500
    } while ((Get-Date) -lt $deadline)
    throw "$Name did not exit within $TimeoutSeconds seconds"
}

function Wait-ConnectedState {
    param([int]$TimeoutSeconds = 45)
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        $stateText = & docker exec $engine aimedia control state --json 2>$null
        if ($LASTEXITCODE -eq 0) {
            $state = ($stateText | ConvertFrom-Json).state
            if ($state.inputs[0].srt.connected -and $state.output.srt.connected) {
                return $state
            }
        }
        Start-Sleep -Milliseconds 500
    } while ((Get-Date) -lt $deadline)
    $logs = Invoke-Docker -Arguments @("logs", "--tail", "120", $engine) -Capture
    throw "soak pipeline did not become connected:`n$($logs -join [Environment]::NewLine)"
}

function Get-Median {
    param([Parameter(Mandatory)][double[]]$Values)
    if ($Values.Count -eq 0) {
        return 0.0
    }
    $ordered = @($Values | Sort-Object)
    $middle = [int][Math]::Floor($ordered.Count / 2)
    if ($ordered.Count % 2 -eq 1) {
        return [double]$ordered[$middle]
    }
    return ([double]$ordered[$middle - 1] + [double]$ordered[$middle]) / 2.0
}

function Get-GpuMemory {
    # The isolated soak is the only GPU workload in its test topology. A single device query is
    # both sufficient for leak detection and less intrusive than spawning two nvidia-smi probes.
    $total = & docker exec $engine nvidia-smi `
        --query-gpu=memory.used --format=csv,noheader,nounits 2>$null
    $totalValues = @($total | ForEach-Object {
        if ($_ -match '^\s*(\d+)\s*$') { [int]$Matches[1] }
    })
    if ($LASTEXITCODE -eq 0 -and $totalValues.Count -gt 0) {
        return [pscustomobject]@{ scope = "device"; mib = [double](($totalValues | Measure-Object -Sum).Sum) }
    }
    return [pscustomobject]@{ scope = "unavailable"; mib = 0.0 }
}

function Get-Sample {
    param([Parameter(Mandatory)][Diagnostics.Stopwatch]$Timer)
    $stateText = Invoke-Docker -Arguments @(
        "exec", $engine, "aimedia", "control", "state", "--json"
    ) -Capture
    $state = (($stateText -join [Environment]::NewLine) | ConvertFrom-Json).state
    $rssKbText = Invoke-Docker -Arguments @(
        "exec", $engine, "sh", "-c", "grep VmRSS /proc/1/status | tr -dc '0-9'"
    ) -Capture
    $rssBytes = [int64]($rssKbText -join "").Trim() * 1024
    $cgroupText = Invoke-Docker -Arguments @(
        "exec", $engine, "cat", "/sys/fs/cgroup/memory.current"
    ) -Capture
    $gpu = Get-GpuMemory
    $sample = [pscustomobject]@{
        elapsedSeconds = [Math]::Round($Timer.Elapsed.TotalSeconds, 3)
        rssBytes = $rssBytes
        cgroupMemoryBytes = [int64]($cgroupText -join "").Trim()
        gpuMemoryScope = $gpu.scope
        gpuMemoryMiB = $gpu.mib
        videoDecodedFrames = $state.inputs[0].codec.videoDecodedFrames
        videoDroppedFrames = $state.inputs[0].codec.videoDroppedFrames
        videoEncodedFrames = $state.output.videoEncodedFrames
        latencySamples = $state.output.engineLatency.samples
        latencyP50Ms = $state.output.engineLatency.p50Ms
        latencyP95Ms = $state.output.engineLatency.p95Ms
        latencyMaxMs = $state.output.engineLatency.maxMs
        gpuSurfacesInUse = $state.inputs[0].gpu.inUse
        gpuSurfaceCapacity = $state.inputs[0].gpu.capacity
        gpuSurfaceHighWatermark = $state.inputs[0].gpu.highWatermark
        maxQueueDepth = [int](($state.queues | Measure-Object -Property depth -Maximum).Maximum)
        maxQueueCapacity = [int](($state.queues | Measure-Object -Property capacity -Maximum).Maximum)
        maxQueueHighWatermark = [int](($state.queues | Measure-Object -Property highWatermark -Maximum).Maximum)
        inputReconnects = $state.inputs[0].srt.reconnects
        outputReconnects = $state.output.srt.reconnects
    }
    [IO.File]::AppendAllText(
        $samplesPath,
        ($sample | ConvertTo-Json -Compress) + [Environment]::NewLine,
        [Text.UTF8Encoding]::new($false)
    )
    return [pscustomobject]@{
        sample = $sample
        state = $state
    }
}

function Test-ZeroRegressions {
    param([Parameter(Mandatory)]$Timing)
    return (
        $Timing.videoPts90khz.samples -gt 0 -and
        $Timing.videoDts90khz.samples -gt 0 -and
        $Timing.audioPts90khz.samples -gt 0 -and
        $Timing.audioDts90khz.samples -gt 0 -and
        $Timing.pcr27mhz.samples -gt 0 -and
        $Timing.videoPts90khz.regressions -eq 0 -and
        $Timing.videoDts90khz.regressions -eq 0 -and
        $Timing.audioPts90khz.regressions -eq 0 -and
        $Timing.audioDts90khz.regressions -eq 0 -and
        $Timing.pcr27mhz.regressions -eq 0
    )
}

function Test-CleanRuntimeImage {
    $check = & docker run --rm --entrypoint sh $EngineImage -c @'
set -eu
if command -v ffmpeg >/dev/null 2>&1 || command -v ffprobe >/dev/null 2>&1; then
    exit 1
fi
if ldd /usr/local/bin/aimedia 2>/dev/null | grep -E 'libav(codec|format|util|filter|device|resample|scale)' >/dev/null; then
    exit 1
fi
'@ 2>&1
    if ($LASTEXITCODE -ne 0) {
        Write-Warning "runtime dependency check failed: $($check -join [Environment]::NewLine)"
        return $false
    }
    return $true
}

$config = @"
apiVersion: aimedia/v1alpha1
kind: DirectorPipeline
metadata:
  name: soak-1080p30
inputs:
  - name: program
    role: custom
    uri: srt://${source}:9001
    srt:
      mode: caller
      latencyMs: 20
      connectTimeoutMs: 10000
      reconnect:
        enabled: true
        initialBackoffMs: 250
        maxBackoffMs: 1000
output:
  uri: srt://0.0.0.0:10000
  srt:
    mode: listener
    latencyMs: 20
    connectTimeoutMs: 10000
    reconnect:
      enabled: true
      initialBackoffMs: 250
      maxBackoffMs: 1000
media:
  video:
    width: 1920
    height: 1080
    fps: 30
    bitrateKbps: 6000
    gopMs: 1000
    profile: main
    bFrames: 0
  audio:
    sampleRate: 48000
    channels: 2
    bitrateKbps: 128
sync:
  masterInput: 0
  bufferMs: 120
  maxSkewMs: 80
control:
  socketPath: /run/aimedia/aimedia.sock
  socketMode: "0660"
"@

try {
    $null = New-Item -ItemType Directory -Path $runRoot
    [IO.File]::WriteAllText($configPath, $config, [Text.UTF8Encoding]::new($false))
    $null = Invoke-Docker -Arguments @("image", "inspect", $EngineImage) -Capture
    $null = Invoke-Docker -Arguments @("image", "inspect", $PeerImage) -Capture
    $cleanRuntimeImage = Test-CleanRuntimeImage
    $null = Invoke-Docker -Arguments @("network", "create", $network) -Capture

    Start-Container -Name $source -Arguments @(
        "--network", $network, "--entrypoint", "ffmpeg", $PeerImage,
        "-hide_banner", "-loglevel", "warning", "-re",
        "-f", "lavfi", "-i", "smptebars=size=1920x1080:rate=30",
        "-f", "lavfi", "-i", "sine=frequency=997:sample_rate=48000",
        "-t", "$($DurationSeconds + 30)",
        "-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency",
        "-profile:v", "main", "-pix_fmt", "yuv420p", "-bf", "0",
        "-g", "30", "-keyint_min", "30", "-sc_threshold", "0", "-b:v", "6000k",
        "-c:a", "aac", "-b:a", "128k", "-ar", "48000", "-ac", "2",
        "-f", "mpegts", "srt://0.0.0.0:9001?mode=listener&latency=20000"
    )
    Start-Container -Name $engine -Arguments @(
        "--network", $network, "--gpus", "all",
        "-e", "NVIDIA_DRIVER_CAPABILITIES=compute,video,utility",
        "-v", "${configPath}:/config/job.yaml:ro",
        $EngineImage, "run", "-f", "/config/job.yaml"
    )
    Start-Sleep -Milliseconds 500
    # Keep the receiver alive beyond the measurement window so final state capture is deterministic.
    $probeDurationMs = [int64]($DurationSeconds + 15) * 1000
    Start-Container -Name $probe -Arguments @(
        "--network", $network, $EngineImage,
        "probe", "srt://${engine}:10000?latency=20000",
        "--mode", "caller", "--duration-ms", "$probeDurationMs", "--json"
    )

    $null = Wait-ConnectedState
    $timer = [Diagnostics.Stopwatch]::StartNew()
    $nextProgress = 0
    $finalState = $null
    while ($timer.Elapsed.TotalSeconds -lt $DurationSeconds) {
        $sampleResult = Get-Sample -Timer $timer
        $sample = $sampleResult.sample
        $finalState = $sampleResult.state
        $samples.Add($sample)
        if ($timer.Elapsed.TotalSeconds -ge $nextProgress) {
            Write-Host (
                "soak {0:n0}/{1}s rss={2:n1}MiB gpu={3:n1}MiB p95={4}ms surfaces={5}/{6}" -f
                $timer.Elapsed.TotalSeconds, $DurationSeconds,
                ($sample.rssBytes / 1MB), $sample.gpuMemoryMiB,
                $sample.latencyP95Ms, $sample.gpuSurfacesInUse, $sample.gpuSurfaceCapacity
            )
            $nextProgress += 60
        }
        $remaining = $DurationSeconds - $timer.Elapsed.TotalSeconds
        if ($remaining -gt 0) {
            Start-Sleep -Seconds ([Math]::Min($SampleIntervalSeconds, $remaining))
        }
    }

    if ($null -eq $finalState) {
        throw "soak collected no runtime state"
    }
    $probeExit = Wait-ContainerExit -Name $probe -TimeoutSeconds 45
    $probeText = Invoke-Docker -Arguments @("logs", $probe) -Capture
    $probeReport = ($probeText -join [Environment]::NewLine) | ConvertFrom-Json
    $null = Invoke-Docker -Arguments @("kill", "--signal=SIGINT", $engine) -Capture
    $engineExit = Wait-ContainerExit -Name $engine -TimeoutSeconds 30
    $null = Invoke-Docker -Arguments @("rm", "-f", $source) -Capture

    $stable = @($samples | Where-Object elapsedSeconds -ge $WarmupSeconds)
    if ($stable.Count -lt 6) {
        throw "soak collected too few post-warmup samples"
    }
    $window = [Math]::Max(3, [Math]::Floor($stable.Count / 10))
    $first = @($stable | Select-Object -First $window)
    $last = @($stable | Select-Object -Last $window)
    $rssStart = Get-Median -Values @($first | ForEach-Object { [double]$_.rssBytes })
    $rssEnd = Get-Median -Values @($last | ForEach-Object { [double]$_.rssBytes })
    $gpuStart = Get-Median -Values @($first | ForEach-Object { [double]$_.gpuMemoryMiB })
    $gpuEnd = Get-Median -Values @($last | ForEach-Object { [double]$_.gpuMemoryMiB })
    $rssGrowth = $rssEnd - $rssStart
    $gpuGrowth = $gpuEnd - $gpuStart
    $rssLimit = [Math]::Max(64MB, $rssStart * 0.10)
    $queueBounded = @(
        $finalState.queues | Where-Object { $_.highWatermark -gt $_.capacity }
    ).Count -eq 0
    $timing = $probeReport.timing
    $pcrGapLimit = 1080000

    $gates = [ordered]@{
        duration = $probeReport.durationMs -ge ([int64]$DurationSeconds * 950)
        latencySamples = $finalState.output.engineLatency.samples -ge ([int64]$DurationSeconds * 27)
        latencyP95 = $finalState.output.engineLatency.p95Ms -le 180
        rssStable = $rssGrowth -le $rssLimit
        gpuMemoryAvailable = $last[-1].gpuMemoryScope -ne "unavailable"
        gpuMemoryStable = $gpuGrowth -le 64
        queuesBounded = $queueBounded
        gpuSurfacesBounded = $finalState.inputs[0].gpu.highWatermark -le $finalState.inputs[0].gpu.capacity
        timestampsMonotonic = Test-ZeroRegressions -Timing $timing
        pcrCadence = $timing.pcr27mhz.samples -gt 0 -and $timing.pcr27mhz.maxGap -le $pcrGapLimit
        firstVideoKeyframe = [bool]$timing.firstVideoKeyframe
        transportClean = (
            $probeReport.continuityErrors -eq 0 -and
            $probeReport.discontinuities -eq 0 -and
            $probeReport.corruptUnits -eq 0
        )
        runtimeDependencyClean = $cleanRuntimeImage
        processesExited = $probeExit -eq 0 -and $engineExit -eq 0
    }
    $failedGates = @($gates.GetEnumerator() | Where-Object { !$_.Value } | ForEach-Object Key)
    $report = [pscustomobject]@{
        schema = "aimedia.soak/v1alpha1"
        createdAt = [DateTimeOffset]::UtcNow.ToString("O")
        engineImage = $EngineImage
        peerImage = $PeerImage
        requestedDurationSeconds = $DurationSeconds
        sampleIntervalSeconds = $SampleIntervalSeconds
        warmupSeconds = $WarmupSeconds
        samples = $samples.Count
        probe = $probeReport
        finalState = $finalState
        memory = [pscustomobject]@{
            rssStartBytes = [int64]$rssStart
            rssEndBytes = [int64]$rssEnd
            rssGrowthBytes = [int64]$rssGrowth
            rssGrowthLimitBytes = [int64]$rssLimit
            gpuScope = $last[-1].gpuMemoryScope
            gpuStartMiB = $gpuStart
            gpuEndMiB = $gpuEnd
            gpuGrowthMiB = $gpuGrowth
        }
        gates = $gates
        passed = $failedGates.Count -eq 0
        failedGates = $failedGates
    }
    $reportPath = Join-Path $runRoot "summary.json"
    [IO.File]::WriteAllText(
        $reportPath,
        ($report | ConvertTo-Json -Depth 12),
        [Text.UTF8Encoding]::new($false)
    )
    Write-Host "soak report: $reportPath"
    $report | ConvertTo-Json -Depth 12
    if ($failedGates.Count -gt 0) {
        throw "soak gates failed: $($failedGates -join ', ')"
    }
}
finally {
    if (!$KeepContainers) {
        foreach ($name in $containers) {
            & docker rm -f $name 2>$null | Out-Null
        }
        & docker network rm $network 2>$null | Out-Null
    } else {
        Write-Host "kept Docker network $network and containers for inspection"
    }
}
