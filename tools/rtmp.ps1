[CmdletBinding()]
param(
    [string]$EngineImage = "aimedia:gpu",
    [string]$PeerImage = "aimedia:test-tools",
    [string]$MediaImage = "bluenviron/mediamtx:1.20.0@sha256:86e63af28616d5e5a18540d7b031b6510bd4cbf1a3c7d224f9e2976f02aefbfb",
    [ValidateRange(120, 86400)]
    [int]$DurationSeconds = 180,
    [ValidateRange(15, 82800)]
    [int]$InputFaultAtSeconds = 45,
    [ValidateRange(5, 120)]
    [int]$InputFaultSeconds = 8,
    [ValidateRange(30, 84000)]
    [int]$OutputFaultAtSeconds = 75,
    [ValidateRange(5, 120)]
    [int]$OutputFaultSeconds = 8,
    [ValidateRange(45, 84600)]
    [int]$ImpairAtSeconds = 110,
    [ValidateRange(5, 300)]
    [int]$ImpairSeconds = 20,
    [ValidateRange(1, 60)]
    [int]$SampleIntervalSeconds = 5,
    [switch]$KeepContainers
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

if ($InputFaultAtSeconds + $InputFaultSeconds + 5 -ge $OutputFaultAtSeconds) {
    throw "the input fault must finish at least 5 seconds before the output fault"
}
if ($OutputFaultAtSeconds + $OutputFaultSeconds + 5 -ge $ImpairAtSeconds) {
    throw "the output fault must finish at least 5 seconds before network impairment"
}
if ($ImpairAtSeconds + $ImpairSeconds + 10 -ge $DurationSeconds) {
    throw "network impairment must finish at least 10 seconds before the run ends"
}

$runId = [Guid]::NewGuid().ToString("N").Substring(0, 8)
$network = "am-rtmp-$runId"
$media = "am-$runId-media"
$source = "am-$runId-source"
$engine = "am-$runId-engine"
$baselineProbe = "am-$runId-baseline"
$recoveryProbe = "am-$runId-recovery"
$runRoot = Join-Path ([IO.Path]::GetTempPath()) "aimedia-rtmp-$runId"
$configPath = Join-Path $runRoot "job.yaml"
$samplesPath = Join-Path $runRoot "samples.jsonl"
$containers = @($source, $baselineProbe, $recoveryProbe, $engine, $media)
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
                $logs = Invoke-Docker -Arguments @("logs", "--tail", "120", $Name) -Capture
                throw "$Name exited with ${exitCode}:`n$($logs -join [Environment]::NewLine)"
            }
            return $exitCode
        }
        Start-Sleep -Milliseconds 500
    } while ((Get-Date) -lt $deadline)
    throw "$Name did not exit within $TimeoutSeconds seconds"
}

$script:lastControlError = "not queried"

function Get-State {
    param(
        [ValidateRange(1, 20)]
        [int]$Attempts = 1,
        [ValidateRange(0, 5000)]
        [int]$DelayMilliseconds = 250
    )

    for ($attempt = 1; $attempt -le $Attempts; $attempt++) {
        $stateText = & docker exec $engine aimedia control state --json 2>&1
        $exitCode = $LASTEXITCODE
        if ($exitCode -eq 0) {
            try {
                $state = (($stateText -join [Environment]::NewLine) | ConvertFrom-Json).state
                $script:lastControlError = ""
                return $state
            }
            catch {
                $script:lastControlError = "invalid control JSON: $($_.Exception.Message)"
            }
        }
        else {
            $detail = (($stateText | ForEach-Object { "$_" }) -join " ").Trim()
            $script:lastControlError = "docker exec exited ${exitCode}: $detail"
        }

        if ($attempt -lt $Attempts -and $DelayMilliseconds -gt 0) {
            Start-Sleep -Milliseconds $DelayMilliseconds
        }
    }
    return $null
}

function Wait-Control {
    param([int]$TimeoutSeconds = 30)

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        $state = Get-State
        if ($null -ne $state -and $state.running) {
            return $state
        }
        Start-Sleep -Milliseconds 250
    } while ((Get-Date) -lt $deadline)
    throw "RTMP control socket did not become ready"
}

function Wait-Ready {
    param(
        [int]$TimeoutSeconds = 60,
        [int64]$MinimumInputReconnects = 0,
        [int64]$MinimumOutputReconnects = 0
    )

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        $state = Get-State
        if (
            $null -ne $state -and
            $state.running -and
            $state.inputs[0].rtmp.connected -and
            $state.inputs[0].rtmp.packetsReceived -gt 0 -and
            $state.inputs[0].rtmp.reconnects -ge $MinimumInputReconnects -and
            $state.output.rtmp.connected -and
            $state.output.rtmp.packetsSent -gt 0 -and
            $state.output.rtmp.reconnects -ge $MinimumOutputReconnects
        ) {
            return $state
        }
        Start-Sleep -Milliseconds 500
    } while ((Get-Date) -lt $deadline)

    $logs = Invoke-Docker -Arguments @("logs", "--tail", "160", $engine) -Capture -AllowFailure
    throw "RTMP pipeline did not become ready:`n$($logs -join [Environment]::NewLine)"
}

function Wait-MediaServer {
    param(
        [int]$TimeoutSeconds = 20,
        [int]$MinimumStarts = 1
    )

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        $logs = Invoke-Docker -Arguments @("logs", $media) -Capture
        $starts = @($logs | Where-Object {
            $_ -match "\[RTMP\].*(started with listener|listener opened).*:1935"
        }).Count
        if ($starts -ge $MinimumStarts) {
            return
        }
        Start-Sleep -Milliseconds 250
    } while ((Get-Date) -lt $deadline)
    throw "MediaMTX RTMP listener did not start"
}

function Start-Source {
    Start-Container -Name $source -Arguments @(
        "--network", $network,
        "--entrypoint", "ffmpeg",
        $PeerImage,
        "-hide_banner", "-loglevel", "warning", "-re",
        "-f", "lavfi", "-i", "smptebars=size=1920x1080:rate=30",
        "-f", "lavfi", "-i", "sine=frequency=997:sample_rate=48000",
        "-t", "$($DurationSeconds + 120)",
        "-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency",
        "-profile:v", "main", "-pix_fmt", "yuv420p", "-bf", "0",
        "-g", "30", "-keyint_min", "30", "-sc_threshold", "0", "-b:v", "6000k",
        "-c:a", "aac", "-b:a", "128k", "-ar", "48000", "-ac", "2",
        "-f", "flv", "rtmp://${engine}:1935/live/camera"
    )
}

function Start-Probe {
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][string]$FileName,
        [int]$Seconds = 15
    )

    Start-Container -Name $Name -Arguments @(
        "--network", $network,
        "-v", "${runRoot}:/work",
        "--entrypoint", "timeout",
        $PeerImage,
        "--signal=INT", "--kill-after=5", "${Seconds}s", "ffmpeg",
        "-hide_banner", "-loglevel", "warning", "-y", "-copyts",
        "-i", "rtmp://${media}:1935/live/program",
        "-map", "0", "-c", "copy", "-copytb", "1",
        "-avoid_negative_ts", "disabled", "-f", "flv", "/work/$FileName"
    )
}

function Wait-Playable {
    param([int]$TimeoutSeconds = 30)

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        $probe = & docker @(
            "run", "--rm", "--network", $network, "--entrypoint", "ffprobe", $PeerImage,
            "-v", "error", "-rw_timeout", "2000000",
            "-select_streams", "v:0", "-show_entries", "stream=codec_name",
            "-of", "default=noprint_wrappers=1:nokey=1",
            "rtmp://${media}:1935/live/program"
        ) 2>$null
        $probeExit = $LASTEXITCODE
        if ($probeExit -eq 0 -and ($probe -join "`n") -match "h264") {
            return
        }
        Start-Sleep -Milliseconds 500
    } while ((Get-Date) -lt $deadline)
    throw "MediaMTX program did not become playable before the deadline"
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

function Get-GpuMemory {
    # This test topology has one GPU workload. Device memory queried inside the container is stable
    # across Docker Desktop PID namespaces and is sufficient for detecting monotonic growth.
    $rows = & docker exec $engine nvidia-smi `
        --query-gpu=memory.used --format=csv,noheader,nounits 2>$null
    if ($LASTEXITCODE -ne 0) {
        return [pscustomobject]@{ scope = "unavailable"; mib = 0.0 }
    }
    $value = 0.0
    foreach ($row in $rows) {
        if ("$row" -match "^\s*([0-9]+(?:\.[0-9]+)?)") {
            $value += [double]$Matches[1]
        }
    }
    return [pscustomobject]@{ scope = "isolated-device"; mib = $value }
}

function Get-Sample {
    param([Parameter(Mandatory)][Diagnostics.Stopwatch]$Timer)

    $state = Get-State -Attempts 5 -DelayMilliseconds 500
    if ($null -eq $state) {
        $engineStatus = (& docker inspect -f "{{.State.Status}}/{{.State.ExitCode}}" $engine 2>&1) `
            -join " "
        throw "control state unavailable after retries; engine=$engineStatus; last=$script:lastControlError"
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
        inputConnected = [bool]$state.inputs[0].rtmp.connected
        inputPackets = Convert-ToInt64 $state.inputs[0].rtmp.packetsReceived "inputPackets"
        inputReconnects = Convert-ToInt64 $state.inputs[0].rtmp.reconnects "inputReconnects"
        outputConnected = [bool]$state.output.rtmp.connected
        outputPackets = Convert-ToInt64 $state.output.rtmp.packetsSent "outputPackets"
        outputReconnects = Convert-ToInt64 $state.output.rtmp.reconnects "outputReconnects"
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

function Test-Monotonic {
    param([Parameter(Mandatory)][AllowEmptyCollection()][object[]]$Packets)

    $last = $null
    foreach ($packet in $Packets) {
        if ($null -eq $packet.pts -or "$($packet.pts)" -eq "N/A") {
            continue
        }
        $value = [int64]$packet.pts
        if ($null -ne $last -and $value -lt $last) {
            return $false
        }
        $last = $value
    }
    return $null -ne $last
}

function Read-Probe {
    param([Parameter(Mandatory)][string]$FileName)

    $output = Invoke-Docker -Arguments @(
        "run", "--rm", "-v", "${runRoot}:/work:ro",
        "--entrypoint", "ffprobe", $PeerImage,
        "-v", "quiet", "-show_streams", "-show_packets", "-of", "json", "/work/$FileName"
    ) -Capture
    $report = ($output -join [Environment]::NewLine) | ConvertFrom-Json
    $video = @($report.packets | Where-Object codec_type -eq "video")
    $audio = @($report.packets | Where-Object codec_type -eq "audio")
    $videoStream = $report.streams | Where-Object codec_type -eq "video" | Select-Object -First 1
    $audioStream = $report.streams | Where-Object codec_type -eq "audio" | Select-Object -First 1
    $firstKeyframe = -1
    for ($index = 0; $index -lt $video.Count; $index++) {
        if ("$($video[$index].flags)" -match "K") {
            $firstKeyframe = $index
            break
        }
    }
    $videoPts = @($video | Where-Object { $null -ne $_.pts -and "$($_.pts)" -ne "N/A" } | ForEach-Object { [int64]$_.pts })
    return [pscustomobject]@{
        file = $FileName
        videoCodec = $videoStream.codec_name
        width = [int]$videoStream.width
        height = [int]$videoStream.height
        audioCodec = $audioStream.codec_name
        sampleRate = [int]$audioStream.sample_rate
        channels = [int]$audioStream.channels
        videoPackets = $video.Count
        audioPackets = $audio.Count
        firstKeyframePacket = $firstKeyframe
        videoMonotonic = Test-Monotonic -Packets $video
        audioMonotonic = Test-Monotonic -Packets $audio
        videoPtsMin = if ($videoPts.Count -gt 0) { ($videoPts | Measure-Object -Minimum).Minimum } else { $null }
        videoPtsMax = if ($videoPts.Count -gt 0) { ($videoPts | Measure-Object -Maximum).Maximum } else { $null }
    }
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
  name: rtmp-interop
inputs:
  - name: encoder
    role: custom
    uri: rtmp://0.0.0.0:1935/live
    rtmp:
      mode: listen
      streamName: camera
      connectTimeoutMs: 3000
      handshakeTimeoutMs: 5000
      readTimeoutMs: 5000
      maxMessageBytes: 8388608
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
    uri: rtmp://${media}:1935/live
    rtmp:
      mode: publish
      streamName: program
      connectTimeoutMs: 3000
      handshakeTimeoutMs: 5000
      readTimeoutMs: 5000
      maxMessageBytes: 8388608
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
    Start-Container -Name $engine -Arguments @(
        "--network", $network, "--gpus", "all",
        "-e", "NVIDIA_DRIVER_CAPABILITIES=compute,video,utility",
        "-v", "${configPath}:/config/job.yaml:ro",
        $EngineImage, "run", "-f", "/config/job.yaml"
    )
    $null = Wait-Control
    Start-Source
    $initialState = Wait-Ready
    $initialInputPackets = [int64]$initialState.inputs[0].rtmp.packetsReceived
    $initialOutputPackets = [int64]$initialState.output.rtmp.packetsSent
    Wait-Playable
    Start-Probe -Name $baselineProbe -FileName "baseline.flv"

    $timer = [Diagnostics.Stopwatch]::StartNew()
    $nextSample = 0.0
    $nextProgress = 0.0
    $inputFaultStarted = $false
    $inputRestarted = $false
    $inputDisconnected = $false
    $inputRecovered = $false
    $outputFaultStarted = $false
    $outputRestarted = $false
    $outputDisconnected = $false
    $outputRecovered = $false
    $recoveryProbeStarted = $false
    $impairmentApplied = $false
    $impairmentRemoved = $false
    $finalState = $null

    while ($timer.Elapsed.TotalSeconds -lt $DurationSeconds) {
        $elapsed = $timer.Elapsed.TotalSeconds
        if (!$inputFaultStarted -and $elapsed -ge $InputFaultAtSeconds) {
            $null = Invoke-Docker -Arguments @("stop", "-t", "1", $source) -Capture
            $null = Invoke-Docker -Arguments @("rm", $source) -Capture
            $inputFaultStarted = $true
            Write-Host "RTMP input publisher stopped at $([Math]::Round($elapsed, 1))s"
        }
        if (
            $inputFaultStarted -and !$inputRestarted -and
            $elapsed -ge ($InputFaultAtSeconds + $InputFaultSeconds)
        ) {
            Start-Source
            $inputRestarted = $true
            Write-Host "RTMP input publisher restarted at $([Math]::Round($elapsed, 1))s"
        }
        if (!$outputFaultStarted -and $elapsed -ge $OutputFaultAtSeconds) {
            $null = Invoke-Docker -Arguments @("stop", "-t", "1", $media) -Capture
            $outputFaultStarted = $true
            Write-Host "RTMP output server stopped at $([Math]::Round($elapsed, 1))s"
        }
        if (
            $outputFaultStarted -and !$outputRestarted -and
            $elapsed -ge ($OutputFaultAtSeconds + $OutputFaultSeconds)
        ) {
            $mediaStarts = @(Invoke-Docker -Arguments @("logs", $media) -Capture |
                Where-Object {
                    $_ -match "\[RTMP\].*(started with listener|listener opened).*:1935"
                }).Count
            $null = Invoke-Docker -Arguments @("start", $media) -Capture
            Wait-MediaServer -MinimumStarts ($mediaStarts + 1)
            $outputRestarted = $true
            Write-Host "RTMP output server restarted at $([Math]::Round($elapsed, 1))s"
        }
        if (!$impairmentApplied -and $elapsed -ge $ImpairAtSeconds) {
            Set-Netem
            $netemActive = $true
            $impairmentApplied = $true
            Write-Host "network impairment applied at $([Math]::Round($elapsed, 1))s"
        }
        if ($netemActive -and $elapsed -ge ($ImpairAtSeconds + $ImpairSeconds)) {
            Set-Netem -Remove
            $netemActive = $false
            $impairmentRemoved = $true
            Write-Host "network impairment removed at $([Math]::Round($elapsed, 1))s"
        }

        $state = Get-State
        if ($null -ne $state) {
            if ($inputFaultStarted -and !$state.inputs[0].rtmp.connected) {
                $inputDisconnected = $true
            }
            if (
                $inputRestarted -and $state.inputs[0].rtmp.connected -and
                $state.inputs[0].rtmp.reconnects -ge 1 -and
                $state.inputs[0].rtmp.packetsReceived -gt $initialInputPackets
            ) {
                $inputRecovered = $true
            }
            if ($outputFaultStarted -and !$state.output.rtmp.connected) {
                $outputDisconnected = $true
            }
            if (
                $outputRestarted -and $state.output.rtmp.connected -and
                $state.output.rtmp.reconnects -ge 1 -and
                $state.output.rtmp.packetsSent -gt $initialOutputPackets
            ) {
                $outputRecovered = $true
            }
            if ($outputRecovered -and !$recoveryProbeStarted) {
                Wait-Playable
                Start-Probe -Name $recoveryProbe -FileName "recovery.flv"
                $recoveryProbeStarted = $true
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
                    "rtmp {0:n0}/{1}s in={2}/{3} out={4}/{5} p95={6}ms" -f
                    $elapsed, $DurationSeconds,
                    $finalState.inputs[0].rtmp.connected,
                    $finalState.inputs[0].rtmp.reconnects,
                    $finalState.output.rtmp.connected,
                    $finalState.output.rtmp.reconnects,
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
        throw "RTMP run collected no runtime state"
    }

    $baselineExit = Wait-ContainerExit -Name $baselineProbe -TimeoutSeconds 30 -AllowFailure
    if (!$recoveryProbeStarted) {
        throw "recovery probe was never started because RTMP output did not recover"
    }
    $recoveryExit = Wait-ContainerExit -Name $recoveryProbe -TimeoutSeconds 30 -AllowFailure
    $baseline = Read-Probe -FileName "baseline.flv"
    $recovery = Read-Probe -FileName "recovery.flv"
    $null = Invoke-Docker -Arguments @("kill", "--signal=SIGINT", $engine) -Capture
    $engineExit = Wait-ContainerExit -Name $engine -TimeoutSeconds 30

    $stable = @($samples | Where-Object elapsedSeconds -ge 30)
    if ($stable.Count -lt 6) {
        throw "RTMP run collected too few post-warmup samples"
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
    $profileValid = (
        $baseline.videoCodec -eq "h264" -and $baseline.audioCodec -eq "aac" -and
        $baseline.width -eq 1920 -and $baseline.height -eq 1080 -and
        $baseline.sampleRate -eq 48000 -and $baseline.channels -eq 2 -and
        $recovery.videoCodec -eq "h264" -and $recovery.audioCodec -eq "aac"
    )

    $gates = [ordered]@{
        duration = $samples[-1].elapsedSeconds -ge ($DurationSeconds - $SampleIntervalSeconds - 1)
        externalFfmpegInput = $finalState.inputs[0].rtmp.packetsReceived -gt $initialInputPackets
        externalMediaMtxOutput = $baseline.videoPackets -gt 0 -and $recovery.videoPackets -gt 0
        inputFaultObserved = $inputFaultStarted -and $inputRestarted -and $inputDisconnected
        inputReconnectRecovered = $inputRecovered -and $finalState.inputs[0].rtmp.reconnects -ge 1
        outputFaultObserved = $outputFaultStarted -and $outputRestarted -and $outputDisconnected
        outputReconnectRecovered = $outputRecovered -and $finalState.output.rtmp.reconnects -ge 1
        networkImpairment = $impairmentApplied -and $impairmentRemoved
        outputConnected = [bool]$finalState.output.rtmp.connected
        mediaProfile = $profileValid
        baselineTimestamps = $baseline.videoMonotonic -and $baseline.audioMonotonic
        recoveryTimestamps = $recovery.videoMonotonic -and $recovery.audioMonotonic
        recoveryKeyframe = $recovery.firstKeyframePacket -ge 0 -and $recovery.firstKeyframePacket -le 30
        programClockContinues = $recovery.videoPtsMin -gt $baseline.videoPtsMax
        latencyP95 = $finalState.output.engineLatency.p95Ms -le 180
        rssStable = $rssGrowth -le $rssLimit
        gpuMemoryAvailable = $last[-1].gpuMemoryScope -ne "unavailable"
        gpuMemoryStable = $gpuGrowth -le 64
        queuesBounded = $queueBounded
        gpuSurfacesBounded = $finalState.inputs[0].gpu.highWatermark -le $finalState.inputs[0].gpu.capacity
        runtimeDependencyClean = $cleanRuntimeImage
        processesExited = $baselineExit -in @(0, 124) -and $recoveryExit -in @(0, 124) -and
            $engineExit -eq 0
    }
    $failedGates = @($gates.GetEnumerator() | Where-Object { !$_.Value } | ForEach-Object Key)
    $report = [pscustomobject]@{
        schema = "aimedia.rtmp-interop/v1alpha1"
        createdAt = [DateTimeOffset]::UtcNow.ToString("O")
        images = [pscustomobject]@{
            engine = [pscustomobject]@{ name = $EngineImage; digest = $engineDigest }
            peer = [pscustomobject]@{ name = $PeerImage; digest = $peerDigest }
            media = [pscustomobject]@{ name = $MediaImage; digest = $mediaDigest }
        }
        requestedDurationSeconds = $DurationSeconds
        inputFault = [pscustomobject]@{
            atSeconds = $InputFaultAtSeconds
            durationSeconds = $InputFaultSeconds
            observedDisconnected = $inputDisconnected
            recovered = $inputRecovered
        }
        outputFault = [pscustomobject]@{
            atSeconds = $OutputFaultAtSeconds
            durationSeconds = $OutputFaultSeconds
            observedDisconnected = $outputDisconnected
            recovered = $outputRecovered
        }
        impairment = [pscustomobject]@{
            atSeconds = $ImpairAtSeconds
            durationSeconds = $ImpairSeconds
            rttMs = 40
            jitterMs = 20
            packetLossPercent = 1
        }
        samples = $samples.Count
        baseline = $baseline
        recovery = $recovery
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
    Write-Host "RTMP report: $reportPath"
    $report | ConvertTo-Json -Depth 12
    if ($failedGates.Count -gt 0) {
        throw "RTMP gates failed: $($failedGates -join ', ')"
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
