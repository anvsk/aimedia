[CmdletBinding()]
param(
    [string]$EngineImage = "aimedia:gpu",
    [string]$PeerImage = "aimedia:test-tools",
    [string]$MediaImage = "bluenviron/mediamtx:1.20.0@sha256:86e63af28616d5e5a18540d7b031b6510bd4cbf1a3c7d224f9e2976f02aefbfb",
    [ValidateSet("h264", "hevc")]
    [string]$VideoCodec = "h264",
    [ValidateRange(90, 86400)]
    [int]$DurationSeconds = 180,
    [ValidateRange(10, 82800)]
    [int]$FaultAtSeconds = 45,
    [ValidateRange(5, 120)]
    [int]$FaultSeconds = 8,
    [ValidateRange(20, 84600)]
    [int]$ImpairAtSeconds = 90,
    [ValidateRange(5, 300)]
    [int]$ImpairSeconds = 20,
    [ValidateRange(1, 60)]
    [int]$SampleIntervalSeconds = 5,
    [switch]$KeepContainers
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

if ($FaultAtSeconds + $FaultSeconds + 15 -ge $DurationSeconds) {
    throw "fault window must finish at least 15 seconds before the run ends"
}
if ($ImpairAtSeconds + $ImpairSeconds + 5 -ge $DurationSeconds) {
    throw "network impairment must finish at least 5 seconds before the run ends"
}

$runId = [Guid]::NewGuid().ToString("N").Substring(0, 8)
$network = "am-rtsp-$runId"
$media = "am-$runId-media"
$source = "am-$runId-source"
$engine = "am-$runId-engine"
$probe = "am-$runId-probe"
$runRoot = Join-Path ([IO.Path]::GetTempPath()) "aimedia-rtsp-$runId"
$configPath = Join-Path $runRoot "job.yaml"
$samplesPath = Join-Path $runRoot "samples.jsonl"
$containers = @($source, $probe, $engine, $media)
$samples = [Collections.Generic.List[object]]::new()

function Invoke-Docker {
    param(
        [Parameter(Mandatory)][string[]]$Arguments,
        [switch]$Capture,
        [switch]$AllowFailure
    )

    $output = & docker @Arguments 2>&1
    $exitCode = $LASTEXITCODE
    if (!$AllowFailure -and $exitCode -ne 0) {
        throw "docker $($Arguments -join ' ') failed:`n$($output -join [Environment]::NewLine)"
    }
    if ($Capture) {
        return $output
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
        $running = (Invoke-Docker -Arguments @(
            "inspect", "-f", "{{.State.Running}}", $Name
        ) -Capture).Trim()
        if ($running -ne "true") {
            $exitCode = [int](Invoke-Docker -Arguments @(
                "inspect", "-f", "{{.State.ExitCode}}", $Name
            ) -Capture)
            if (!$AllowFailure -and $exitCode -ne 0) {
                $logs = Invoke-Docker -Arguments @("logs", "--tail", "160", $Name) -Capture
                throw "$Name exited with ${exitCode}:`n$($logs -join [Environment]::NewLine)"
            }
            return $exitCode
        }
        Start-Sleep -Milliseconds 500
    } while ((Get-Date) -lt $deadline)
    throw "$Name did not exit within $TimeoutSeconds seconds"
}

function Get-State {
    $stateText = & docker exec $engine aimedia control state --json 2>$null
    if ($LASTEXITCODE -ne 0) {
        return $null
    }
    $response = ($stateText -join [Environment]::NewLine) | ConvertFrom-Json
    return $response.state
}

function Convert-ToInt64 {
    param(
        [AllowNull()]$Value,
        [Parameter(Mandatory)][string]$Name
    )

    $values = @($Value)
    if ($values.Count -ne 1) {
        throw "$Name must be a scalar, received $($values.Count) values"
    }
    return [int64]$values[0]
}

function Wait-Ready {
    param(
        [int]$TimeoutSeconds = 60,
        [int64]$MinimumPackets = 1,
        [int64]$MinimumReconnects = 0
    )

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        $state = Get-State
        if (
            $null -ne $state -and
            $state.running -and
            $state.inputs[0].rtsp.connected -and
            $state.inputs[0].rtsp.packetsReceived -ge $MinimumPackets -and
            $state.inputs[0].rtsp.reconnects -ge $MinimumReconnects -and
            $state.output.srt.connected
        ) {
            return $state
        }
        Start-Sleep -Milliseconds 500
    } while ((Get-Date) -lt $deadline)

    $logs = Invoke-Docker -Arguments @("logs", "--tail", "160", $engine) -Capture -AllowFailure
    throw "RTSP pipeline did not become ready:`n$($logs -join [Environment]::NewLine)"
}

function Wait-Publisher {
    param([int]$TimeoutSeconds = 30)

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        $logs = Invoke-Docker -Arguments @("logs", $media) -Capture
        if (($logs -join "`n") -match "stream is available and online") {
            return
        }
        Start-Sleep -Milliseconds 500
    } while ((Get-Date) -lt $deadline)
    throw "MediaMTX did not observe the RTSP publisher"
}

function Wait-MediaServer {
    param([int]$TimeoutSeconds = 20)

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        $logs = Invoke-Docker -Arguments @("logs", $media) -Capture
        if (($logs -join "`n") -match "\[RTSP\] started with listeners") {
            return
        }
        Start-Sleep -Milliseconds 250
    } while ((Get-Date) -lt $deadline)
    throw "MediaMTX RTSP listener did not start"
}

function Start-Source {
    $arguments = @(
        "--network", $network,
        "--entrypoint", "ffmpeg",
        $PeerImage,
        "-hide_banner", "-loglevel", "warning", "-re",
        "-f", "lavfi", "-i", "smptebars=size=1920x1080:rate=30",
        "-f", "lavfi", "-i", "sine=frequency=997:sample_rate=48000",
        "-t", "$($DurationSeconds + 90)"
    )
    if ($VideoCodec -eq "hevc") {
        $arguments += @(
            "-c:v", "libx265", "-preset", "ultrafast", "-tune", "zerolatency",
            "-profile:v", "main", "-pix_fmt", "yuv420p", "-b:v", "6000k",
            "-x265-params", "bframes=0:keyint=30:min-keyint=30:scenecut=0:repeat-headers=1"
        )
    }
    else {
        $arguments += @(
            "-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency",
            "-profile:v", "main", "-pix_fmt", "yuv420p", "-bf", "0",
            "-g", "30", "-keyint_min", "30", "-sc_threshold", "0", "-b:v", "6000k"
        )
    }
    $arguments += @(
        "-c:a", "aac", "-b:a", "128k", "-ar", "48000", "-ac", "2",
        "-f", "rtsp", "-rtsp_transport", "tcp",
        "rtsp://${media}:8554/camera"
    )
    Start-Container -Name $source -Arguments $arguments
}

function Set-Netem {
    param([switch]$Remove)

    $operation = if ($Remove) { "del" } else { "replace" }
    $arguments = @(
        "run", "--rm", "--network", "container:$engine", "--cap-add", "NET_ADMIN",
        "--entrypoint", "tc", $PeerImage,
        "qdisc", $operation, "dev", "eth0", "root"
    )
    if (!$Remove) {
        $arguments += @("netem", "delay", "40ms", "20ms", "distribution", "normal", "loss", "1%")
    }
    $null = Invoke-Docker -Arguments $arguments -Capture
}

function Get-GpuMemory {
    $pidText = Invoke-Docker -Arguments @(
        "inspect", "-f", "{{.State.Pid}}", $engine
    ) -Capture
    $containerPid = [int64]($pidText -join "").Trim()
    $rows = & nvidia-smi --query-compute-apps=pid,used_memory --format=csv,noheader,nounits 2>$null
    if ($LASTEXITCODE -ne 0) {
        return [pscustomobject]@{ scope = "unavailable"; mib = 0.0 }
    }
    $value = 0.0
    foreach ($row in $rows) {
        $parts = $row -split ","
        if ($parts.Count -eq 2 -and [int64]$parts[0].Trim() -eq $containerPid) {
            $value += [double]$parts[1].Trim()
        }
    }
    return [pscustomobject]@{ scope = "container-pid"; mib = $value }
}

function Get-Sample {
    param([Parameter(Mandatory)][Diagnostics.Stopwatch]$Timer)

    $state = Get-State
    if ($null -eq $state) {
        throw "control state became unavailable while the pipeline was running"
    }
    $status = Invoke-Docker -Arguments @("exec", $engine, "cat", "/proc/1/status") -Capture
    $rssLine = $status | Where-Object { $_ -match "^VmRSS:" } | Select-Object -First 1
    $rssKiB = Convert-ToInt64 `
        -Value (($rssLine -replace "^VmRSS:\s+", "") -replace "\s+kB$", "") `
        -Name "VmRSS"
    $gpu = Get-GpuMemory
    $sample = [pscustomobject]@{
        elapsedSeconds = [Math]::Round($Timer.Elapsed.TotalSeconds, 3)
        rssBytes = $rssKiB * 1KB
        gpuMemoryScope = $gpu.scope
        gpuMemoryMiB = $gpu.mib
        rtspConnected = [bool]$state.inputs[0].rtsp.connected
        rtspPackets = Convert-ToInt64 $state.inputs[0].rtsp.packetsReceived "rtspPackets"
        rtspLost = Convert-ToInt64 $state.inputs[0].rtsp.packetsLost "rtspLost"
        rtspReconnects = Convert-ToInt64 $state.inputs[0].rtsp.reconnects "rtspReconnects"
        videoDecodedFrames = Convert-ToInt64 $state.inputs[0].codec.videoDecodedFrames "videoDecodedFrames"
        audioDecodedFrames = Convert-ToInt64 $state.inputs[0].codec.audioDecodedFrames "audioDecodedFrames"
        videoEncodedFrames = Convert-ToInt64 $state.output.videoEncodedFrames "videoEncodedFrames"
        latencyP95Ms = Convert-ToInt64 $state.output.engineLatency.p95Ms "latencyP95Ms"
        maxQueueDepth = [int](($state.queues | Measure-Object -Property depth -Maximum).Maximum)
        maxQueueCapacity = [int](($state.queues | Measure-Object -Property capacity -Maximum).Maximum)
        maxQueueHighWatermark = [int](($state.queues | Measure-Object -Property highWatermark -Maximum).Maximum)
        gpuSurfacesInUse = [int]$state.inputs[0].gpu.inUse
        gpuSurfaceCapacity = [int]$state.inputs[0].gpu.capacity
        gpuSurfaceHighWatermark = [int]$state.inputs[0].gpu.highWatermark
    }
    [IO.File]::AppendAllText(
        $samplesPath,
        ($sample | ConvertTo-Json -Compress) + [Environment]::NewLine,
        [Text.UTF8Encoding]::new($false)
    )
    return [pscustomobject]@{ sample = $sample; state = $state }
}

function Get-Median {
    param([Parameter(Mandatory)][double[]]$Values)

    $sorted = @($Values | Sort-Object)
    if ($sorted.Count % 2 -eq 1) {
        return $sorted[[Math]::Floor($sorted.Count / 2)]
    }
    $upper = $sorted.Count / 2
    return ($sorted[$upper - 1] + $sorted[$upper]) / 2
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
    return $LASTEXITCODE -eq 0
}

$config = @"
apiVersion: aimedia/v1alpha2
kind: MediaJob
metadata:
  name: rtsp-$VideoCodec-interop
inputs:
  - name: camera
    role: custom
    uri: rtsp://${media}:8554/camera
    rtsp:
      transport: tcp
      connectTimeoutMs: 5000
      readTimeoutMs: 3000
      keepaliveMs: 15000
      reconnect:
        enabled: true
        initialBackoffMs: 250
        maxBackoffMs: 1000
processing:
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
  timing:
    masterInput: 0
    bufferMs: 120
    maxSkewMs: 80
outputs:
  - name: program
    uri: srt://0.0.0.0:10000
    srt:
      mode: listener
      latencyMs: 20
      connectTimeoutMs: 10000
      reconnect:
        enabled: true
        initialBackoffMs: 250
        maxBackoffMs: 1000
taps: []
control:
  socketPath: /run/aimedia/aimedia.sock
  socketMode: "0660"
"@

$netemActive = $false
try {
    $null = New-Item -ItemType Directory -Path $runRoot
    [IO.File]::WriteAllText($configPath, $config, [Text.UTF8Encoding]::new($false))
    foreach ($image in @($EngineImage, $PeerImage, $MediaImage)) {
        $null = Invoke-Docker -Arguments @("image", "inspect", $image) -Capture
    }
    $engineDigest = (Invoke-Docker -Arguments @(
        "image", "inspect", $EngineImage, "--format", "{{.Id}}"
    ) -Capture).Trim()
    $peerDigest = (Invoke-Docker -Arguments @(
        "image", "inspect", $PeerImage, "--format", "{{.Id}}"
    ) -Capture).Trim()
    $mediaDigest = (Invoke-Docker -Arguments @(
        "image", "inspect", $MediaImage, "--format", "{{.Id}}"
    ) -Capture).Trim()
    $cleanRuntimeImage = Test-CleanRuntimeImage
    $null = Invoke-Docker -Arguments @("network", "create", $network) -Capture

    Start-Container -Name $media -Arguments @("--network", $network, $MediaImage)
    Wait-MediaServer
    Start-Source
    Wait-Publisher
    Start-Container -Name $engine -Arguments @(
        "--network", $network, "--gpus", "all",
        "-e", "NVIDIA_DRIVER_CAPABILITIES=compute,video,utility",
        "-v", "${configPath}:/config/job.yaml:ro",
        $EngineImage, "run", "-f", "/config/job.yaml"
    )
    Start-Sleep -Milliseconds 500
    $probeDurationMs = [int64]($DurationSeconds + 15) * 1000
    Start-Container -Name $probe -Arguments @(
        "--network", $network, $EngineImage,
        "probe", "srt://${engine}:10000?latency=20000",
        "--mode", "caller", "--duration-ms", "$probeDurationMs", "--json"
    )

    $initialState = Wait-Ready
    $initialPackets = [int64]$initialState.inputs[0].rtsp.packetsReceived
    $timer = [Diagnostics.Stopwatch]::StartNew()
    $nextSample = 0.0
    $nextProgress = 0.0
    $faultStarted = $false
    $sourceRestarted = $false
    $sawDisconnected = $false
    $recovered = $false
    $impairmentApplied = $false
    $impairmentRemoved = $false
    $finalState = $null

    while ($timer.Elapsed.TotalSeconds -lt $DurationSeconds) {
        $elapsed = $timer.Elapsed.TotalSeconds
        if (!$faultStarted -and $elapsed -ge $FaultAtSeconds) {
            $null = Invoke-Docker -Arguments @("stop", "-t", "1", $source) -Capture
            $null = Invoke-Docker -Arguments @("rm", $source) -Capture
            $faultStarted = $true
            Write-Host "RTSP publisher stopped at $([Math]::Round($elapsed, 1))s"
        }
        if (
            $faultStarted -and !$sourceRestarted -and
            $elapsed -ge ($FaultAtSeconds + $FaultSeconds)
        ) {
            Start-Source
            $sourceRestarted = $true
            Write-Host "RTSP publisher restarted at $([Math]::Round($elapsed, 1))s"
        }
        if (!$impairmentApplied -and $elapsed -ge $ImpairAtSeconds) {
            Set-Netem
            $netemActive = $true
            $impairmentApplied = $true
            Write-Host "network impairment applied at $([Math]::Round($elapsed, 1))s"
        }
        if (
            $netemActive -and
            $elapsed -ge ($ImpairAtSeconds + $ImpairSeconds)
        ) {
            Set-Netem -Remove
            $netemActive = $false
            $impairmentRemoved = $true
            Write-Host "network impairment removed at $([Math]::Round($elapsed, 1))s"
        }

        $state = Get-State
        if ($null -ne $state) {
            if (!$state.inputs[0].rtsp.connected) {
                $sawDisconnected = $true
            }
            if (
                $sourceRestarted -and
                $state.inputs[0].rtsp.connected -and
                $state.inputs[0].rtsp.reconnects -ge 1 -and
                $state.inputs[0].rtsp.packetsReceived -gt $initialPackets
            ) {
                $recovered = $true
            }
        }

        if ($elapsed -ge $nextSample) {
            $sampleResult = Get-Sample -Timer $timer
            $samples.Add($sampleResult.sample)
            $finalState = $sampleResult.state
            $nextSample += $SampleIntervalSeconds
        }
        if ($elapsed -ge $nextProgress) {
            if ($null -ne $finalState) {
                Write-Host (
                    "rtsp {0:n0}/{1}s connected={2} reconnects={3} packets={4} p95={5}ms" -f
                    $elapsed, $DurationSeconds, $finalState.inputs[0].rtsp.connected,
                    $finalState.inputs[0].rtsp.reconnects,
                    $finalState.inputs[0].rtsp.packetsReceived,
                    $finalState.output.engineLatency.p95Ms
                )
            }
            $nextProgress += 60
        }
        Start-Sleep -Milliseconds 250
    }

    if ($netemActive) {
        Set-Netem -Remove
        $netemActive = $false
        $impairmentRemoved = $true
    }
    if ($null -eq $finalState) {
        throw "RTSP run collected no runtime state"
    }

    $probeExit = Wait-ContainerExit -Name $probe -TimeoutSeconds 45
    $probeText = Invoke-Docker -Arguments @("logs", $probe) -Capture
    $probeReport = ($probeText -join [Environment]::NewLine) | ConvertFrom-Json
    $null = Invoke-Docker -Arguments @("kill", "--signal=SIGINT", $engine) -Capture
    $engineExit = Wait-ContainerExit -Name $engine -TimeoutSeconds 30

    $stable = @($samples | Where-Object elapsedSeconds -ge 30)
    if ($stable.Count -lt 6) {
        throw "RTSP run collected too few post-warmup samples"
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
        externalRtsp = $finalState.inputs[0].rtsp.packetsReceived -gt $initialPackets
        faultObserved = $faultStarted -and $sourceRestarted -and $sawDisconnected
        reconnectRecovered = $recovered -and $finalState.inputs[0].rtsp.reconnects -ge 1
        networkImpairment = $impairmentApplied -and $impairmentRemoved
        outputConnected = [bool]$finalState.output.srt.connected
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
        schema = "aimedia.rtsp-interop/v1alpha1"
        createdAt = [DateTimeOffset]::UtcNow.ToString("O")
        images = [pscustomobject]@{
            engine = [pscustomobject]@{ name = $EngineImage; digest = $engineDigest }
            peer = [pscustomobject]@{ name = $PeerImage; digest = $peerDigest }
            media = [pscustomobject]@{ name = $MediaImage; digest = $mediaDigest }
        }
        requestedDurationSeconds = $DurationSeconds
        inputVideoCodec = $VideoCodec
        fault = [pscustomobject]@{
            atSeconds = $FaultAtSeconds
            durationSeconds = $FaultSeconds
            observedDisconnected = $sawDisconnected
            recovered = $recovered
        }
        impairment = [pscustomobject]@{
            atSeconds = $ImpairAtSeconds
            durationSeconds = $ImpairSeconds
            rttMs = 40
            jitterMs = 20
            packetLossPercent = 1
        }
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
    Write-Host "RTSP report: $reportPath"
    $report | ConvertTo-Json -Depth 12
    if ($failedGates.Count -gt 0) {
        throw "RTSP gates failed: $($failedGates -join ', ')"
    }
}
finally {
    if ($netemActive) {
        try {
            Set-Netem -Remove
        }
        catch {
            Write-Warning "could not remove netem during cleanup: $($_.Exception.Message)"
        }
    }
    if (!$KeepContainers) {
        foreach ($name in $containers) {
            & docker rm -f $name 2>$null | Out-Null
        }
        & docker network rm $network 2>$null | Out-Null
    }
    else {
        Write-Host "kept Docker network $network and containers for inspection"
    }
}
