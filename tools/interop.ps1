[CmdletBinding()]
param(
    [string]$EngineImage = "aimedia:gpu",
    [string]$PeerImage = "aimedia:test-tools",
    [string]$DesktopImage = "aimedia:desktop-tools",
    [string]$AptMirror = "",
    [ValidateRange(4, 120)]
    [int]$DurationSeconds = 8,
    [ValidateSet("all", "matrix", "ll", "lc", "cl", "cc", "netem", "corrupt", "desktop", "obs", "obs-input", "obs-output", "vlc")]
    [string]$Suite = "all",
    [switch]$SkipToolBuild,
    [switch]$KeepContainers
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
$runId = [Guid]::NewGuid().ToString("N").Substring(0, 8)
$network = "am-io-$runId"
$runRoot = Join-Path ([IO.Path]::GetTempPath()) "aimedia-interop-$runId"
$containers = [Collections.Generic.List[string]]::new()
$results = [Collections.Generic.List[object]]::new()
$desktopResults = [Collections.Generic.List[object]]::new()
$corruptResult = $null
$toolVersions = [ordered]@{}
$needsDesktop = $Suite -in @("all", "desktop", "obs", "obs-input", "obs-output", "vlc")

function Invoke-Docker {
    param(
        [Parameter(Mandatory)]
        [string[]]$Arguments,
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

function Start-TestContainer {
    param(
        [Parameter(Mandatory)]
        [string]$Name,
        [Parameter(Mandatory)]
        [string[]]$Arguments
    )

    $null = Invoke-Docker -Arguments (@("run", "-d", "--name", $Name) + $Arguments) -Capture
    $containers.Add($Name)
}

function Wait-ContainerExit {
    param(
        [Parameter(Mandatory)]
        [string]$Name,
        [int]$TimeoutSeconds = 30,
        [switch]$AllowFailure
    )

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        $running = (Invoke-Docker -Arguments @("inspect", "-f", "{{.State.Running}}", $Name) -Capture).Trim()
        if ($running -ne "true") {
            $exitCode = [int](Invoke-Docker -Arguments @("inspect", "-f", "{{.State.ExitCode}}", $Name) -Capture)
            if (!$AllowFailure -and $exitCode -ne 0) {
                $logs = Invoke-Docker -Arguments @("logs", "--tail", "80", $Name) -Capture
                throw "$Name exited with ${exitCode}:`n$($logs -join [Environment]::NewLine)"
            }
            return $exitCode
        }
        Start-Sleep -Milliseconds 500
    } while ((Get-Date) -lt $deadline)

    throw "$Name did not exit within $TimeoutSeconds seconds"
}

function Wait-ControlState {
    param(
        [Parameter(Mandatory)]
        [string]$Engine,
        [int]$TimeoutSeconds = 20,
        [switch]$RequireConnected
    )

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        $stateText = & docker exec $Engine aimedia control state --json 2>$null
        if ($LASTEXITCODE -eq 0) {
            $state = ($stateText | ConvertFrom-Json).state
            if (!$RequireConnected -or ($state.inputs[0].srt.connected -and $state.output.srt.connected)) {
                return $state
            }
        }
        Start-Sleep -Milliseconds 500
    } while ((Get-Date) -lt $deadline)

    $logs = Invoke-Docker -Arguments @("logs", "--tail", "80", $Engine) -Capture
    throw "control socket for $Engine was not ready:`n$($logs -join [Environment]::NewLine)"
}

function Set-Netem {
    param([Parameter(Mandatory)][string[]]$Targets)

    foreach ($target in $Targets) {
        Invoke-Docker -Arguments @(
            "run", "--rm", "--network", "container:$target", "--cap-add", "NET_ADMIN",
            "--entrypoint", "tc", $PeerImage,
            "qdisc", "replace", "dev", "eth0", "root", "netem",
            "delay", "20ms", "20ms", "distribution", "normal", "loss", "1%"
        )
    }
}

function Test-MonotonicPackets {
    param([Parameter(Mandatory)]$Packets)

    $last = @{}
    foreach ($packet in $Packets) {
        foreach ($field in @("pts", "dts")) {
            $value = $packet.$field
            if ($null -eq $value -or $value -eq "N/A") {
                continue
            }
            $key = "$($packet.codec_type):$field"
            $number = [long]$value
            if ($last.ContainsKey($key) -and $number -lt $last[$key]) {
                throw "$key regressed from $($last[$key]) to $number"
            }
            $last[$key] = $number
        }
    }
}

function Read-OutputProbe {
    param(
        [Parameter(Mandatory)][string]$ScenarioRoot,
        [string]$FileName = "output.ts"
    )

    $probeText = Invoke-Docker -Arguments @(
        "run", "--rm", "-v", "${ScenarioRoot}:/work:ro", "--entrypoint", "ffprobe",
        $PeerImage, "-v", "error", "-show_streams", "-show_packets", "-of", "json",
        "/work/$FileName"
    ) -Capture
    $probe = ($probeText -join [Environment]::NewLine) | ConvertFrom-Json
    Test-MonotonicPackets -Packets $probe.packets
    $video = $probe.streams | Where-Object codec_type -eq "video" | Select-Object -First 1
    $audio = $probe.streams | Where-Object codec_type -eq "audio" | Select-Object -First 1
    if ($video.codec_name -ne "h264" -or $audio.codec_name -ne "aac") {
        throw "$FileName output codecs were $($video.codec_name)/$($audio.codec_name)"
    }
    $firstVideo = $probe.packets | Where-Object codec_type -eq "video" | Select-Object -First 1
    if ($firstVideo.flags -notlike "*K*") {
        throw "$FileName output did not begin on a video keyframe"
    }
    return [pscustomobject]@{
        videoPackets = @($probe.packets | Where-Object codec_type -eq "video").Count
        audioPackets = @($probe.packets | Where-Object codec_type -eq "audio").Count
        firstVideoKeyframe = $true
        monotonicPtsDts = $true
    }
}

function Write-JobConfig {
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][ValidateSet("caller", "listener")][string]$InputMode,
        [Parameter(Mandatory)][ValidateSet("caller", "listener")][string]$OutputMode,
        [Parameter(Mandatory)][int]$InputPort,
        [Parameter(Mandatory)][int]$OutputPort,
        [Parameter(Mandatory)][string]$Producer,
        [Parameter(Mandatory)][string]$Receiver
    )

    $inputUri = if ($InputMode -eq "caller") {
        "srt://${Producer}:$InputPort"
    } else {
        "srt://0.0.0.0:$InputPort"
    }
    $outputUri = if ($OutputMode -eq "caller") {
        "srt://${Receiver}:$OutputPort"
    } else {
        "srt://0.0.0.0:$OutputPort"
    }
    $config = @"
apiVersion: aimedia/v1alpha1
kind: DirectorPipeline
metadata:
  name: interop-$InputMode-$OutputMode
inputs:
  - name: program
    role: custom
    uri: $inputUri
    srt:
      mode: $InputMode
      latencyMs: 120
      connectTimeoutMs: 10000
      reconnect:
        enabled: true
        initialBackoffMs: 250
        maxBackoffMs: 1000
output:
  uri: $outputUri
  srt:
    mode: $OutputMode
    latencyMs: 120
    connectTimeoutMs: 10000
    reconnect:
      enabled: true
      initialBackoffMs: 250
      maxBackoffMs: 1000
media:
  video:
    width: 1280
    height: 720
    fps: 30
    bitrateKbps: 3000
    gopMs: 1000
    profile: main
    bFrames: 0
  audio:
    sampleRate: 48000
    channels: 2
    bitrateKbps: 128
sync:
  masterInput: 0
  bufferMs: 1000
  maxSkewMs: 80
control:
  socketPath: /run/aimedia/aimedia.sock
  socketMode: "0660"
"@
    [IO.File]::WriteAllText($Path, $config, [Text.UTF8Encoding]::new($false))
}

function Start-Producer {
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][ValidateSet("caller", "listener")][string]$PeerMode,
        [Parameter(Mandatory)][string]$Engine,
        [Parameter(Mandatory)][int]$Port,
        [int]$Seconds = $DurationSeconds
    )

    $uri = if ($PeerMode -eq "caller") {
        "srt://${Engine}:$Port`?mode=caller&latency=120000"
    } else {
        "srt://0.0.0.0:$Port`?mode=listener&latency=120000"
    }
    Start-TestContainer -Name $Name -Arguments @(
        "--network", $network, $PeerImage, "ffmpeg",
        "-hide_banner", "-loglevel", "warning", "-re",
        "-f", "lavfi", "-i", "smptebars=size=1280x720:rate=30",
        "-f", "lavfi", "-i", "sine=frequency=1300:sample_rate=48000",
        "-t", "$Seconds",
        "-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency",
        "-profile:v", "main", "-pix_fmt", "yuv420p", "-bf", "0",
        "-g", "30", "-keyint_min", "30", "-sc_threshold", "0", "-b:v", "3000k",
        "-c:a", "aac", "-b:a", "128k", "-ar", "48000", "-ac", "2",
        "-f", "mpegts", $uri
    )
}

function Start-Receiver {
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][ValidateSet("caller", "listener")][string]$PeerMode,
        [Parameter(Mandatory)][string]$Engine,
        [Parameter(Mandatory)][int]$Port,
        [Parameter(Mandatory)][string]$ScenarioRoot
    )

    $uri = if ($PeerMode -eq "caller") {
        "srt://${Engine}:$Port`?mode=caller&latency=120000"
    } else {
        "srt://0.0.0.0:$Port`?mode=listener&latency=120000"
    }
    Start-TestContainer -Name $Name -Arguments @(
        "--network", $network, "-v", "${ScenarioRoot}:/work", $PeerImage, "ffmpeg",
        "-hide_banner", "-loglevel", "warning", "-y", "-rw_timeout", "15000000",
        "-i", $uri, "-t", "$($DurationSeconds + 2)",
        "-c", "copy", "-f", "mpegts", "/work/output.ts"
    )
}

function Invoke-Scenario {
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][ValidateSet("caller", "listener")][string]$InputMode,
        [Parameter(Mandatory)][ValidateSet("caller", "listener")][string]$OutputMode,
        [switch]$Impaired
    )

    $scenarioRoot = Join-Path $runRoot $Name
    $null = New-Item -ItemType Directory -Path $scenarioRoot
    $engine = "am-$runId-$Name-engine"
    $producer = "am-$runId-$Name-source"
    $receiver = "am-$runId-$Name-sink"
    $inputPort = 9001
    $outputPort = 10000
    $configPath = Join-Path $scenarioRoot "job.yaml"
    Write-JobConfig -Path $configPath -InputMode $InputMode -OutputMode $OutputMode `
        -InputPort $inputPort -OutputPort $outputPort -Producer $producer -Receiver $receiver

    if ($InputMode -eq "caller") {
        Start-Producer -Name $producer -PeerMode listener -Engine $engine -Port $inputPort
    }
    if ($OutputMode -eq "caller") {
        Start-Receiver -Name $receiver -PeerMode listener -Engine $engine -Port $outputPort `
            -ScenarioRoot $scenarioRoot
    }

    Start-TestContainer -Name $engine -Arguments @(
        "--network", $network, "--gpus", "all",
        "-e", "NVIDIA_DRIVER_CAPABILITIES=compute,video,utility",
        "-v", "${configPath}:/config/job.yaml:ro",
        $EngineImage, "run", "-f", "/config/job.yaml"
    )
    Start-Sleep -Milliseconds 500

    if ($InputMode -eq "listener") {
        Start-Producer -Name $producer -PeerMode caller -Engine $engine -Port $inputPort
    }
    if ($OutputMode -eq "listener") {
        Start-Receiver -Name $receiver -PeerMode caller -Engine $engine -Port $outputPort `
            -ScenarioRoot $scenarioRoot
    }
    if ($Impaired) {
        Set-Netem -Targets @($engine, $producer, $receiver)
    }

    $state = Wait-ControlState -Engine $engine
    Start-Sleep -Seconds ([Math]::Min(3, [Math]::Max(1, $DurationSeconds - 2)))
    $state = Wait-ControlState -Engine $engine
    $sourceExit = Wait-ContainerExit -Name $producer -TimeoutSeconds ($DurationSeconds + 25)
    Start-Sleep -Seconds 1
    $null = Invoke-Docker -Arguments @("kill", "--signal=SIGINT", $engine) -Capture
    $engineExit = Wait-ContainerExit -Name $engine -TimeoutSeconds 25
    $sinkExit = Wait-ContainerExit -Name $receiver -TimeoutSeconds 20 -AllowFailure

    $probe = Read-OutputProbe -ScenarioRoot $scenarioRoot

    $results.Add([pscustomobject]@{
        scenario = $Name
        inputMode = $InputMode
        outputMode = $OutputMode
        impaired = [bool]$Impaired
        sourceExit = $sourceExit
        engineExit = $engineExit
        sinkExit = $sinkExit
        inputConnected = $state.inputs[0].srt.connected
        inputRttMs = $state.inputs[0].srt.rttMs
        inputPacketsLost = $state.inputs[0].srt.packetsLost
        inputPacketsRetransmitted = $state.inputs[0].srt.packetsRetransmitted
        outputConnected = $state.output.srt.connected
        outputRttMs = $state.output.srt.rttMs
        outputPacketsLost = $state.output.srt.packetsLost
        outputPacketsRetransmitted = $state.output.srt.packetsRetransmitted
        videoPackets = $probe.videoPackets
        audioPackets = $probe.audioPackets
        firstVideoKeyframe = $probe.firstVideoKeyframe
        monotonicPtsDts = $probe.monotonicPtsDts
    })
}

function Invoke-CorruptProbe {
    $scenarioRoot = Join-Path $runRoot "corrupt"
    $null = New-Item -ItemType Directory -Path $scenarioRoot
    Invoke-Docker -Arguments @(
        "run", "--rm", "-v", "${scenarioRoot}:/work", $PeerImage, "ffmpeg",
        "-hide_banner", "-loglevel", "error", "-y",
        "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=30",
        "-f", "lavfi", "-i", "sine=frequency=900:sample_rate=48000",
        "-t", "2", "-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency",
        "-profile:v", "main", "-pix_fmt", "yuv420p", "-bf", "0", "-g", "30",
        "-c:a", "aac", "-b:a", "128k", "-ar", "48000", "-ac", "2",
        "-f", "mpegts", "/work/clean.ts"
    )

    $clean = [IO.File]::ReadAllBytes((Join-Path $scenarioRoot "clean.ts"))
    $offset = 188 * 120
    if ($clean.Length -le $offset) {
        throw "generated TS is too short for the corruption fixture"
    }
    $garbage = [byte[]](1..17)
    $corrupt = [byte[]]::new($clean.Length + $garbage.Length)
    [Array]::Copy($clean, 0, $corrupt, 0, $offset)
    [Array]::Copy($garbage, 0, $corrupt, $offset, $garbage.Length)
    [Array]::Copy(
        $clean,
        $offset,
        $corrupt,
        $offset + $garbage.Length,
        $clean.Length - $offset
    )
    [IO.File]::WriteAllBytes((Join-Path $scenarioRoot "corrupt.ts"), $corrupt)

    $probe = "am-$runId-corrupt-probe"
    $source = "am-$runId-corrupt-source"
    Start-TestContainer -Name $source -Arguments @(
        "--network", $network, "-v", "${scenarioRoot}:/work:ro", $PeerImage,
        "sh", "-c",
        'while true; do cat /work/corrupt.ts; sleep 1; done | ffmpeg -hide_banner -loglevel error -f data -i pipe:0 -map 0 -c copy -f data "srt://0.0.0.0:19001?mode=listener&latency=120000"'
    )
    Start-Sleep -Milliseconds 500
    Start-TestContainer -Name $probe -Arguments @(
        "--network", $network, $EngineImage,
        "probe", "srt://${source}:19001", "--mode", "caller", "--duration-ms", "5000", "--json"
    )
    $probeExit = Wait-ContainerExit -Name $probe -TimeoutSeconds 20
    $null = Invoke-Docker -Arguments @("rm", "-f", $source) -Capture
    $probeText = Invoke-Docker -Arguments @("logs", $probe) -Capture
    $probeReport = ($probeText -join [Environment]::NewLine) | ConvertFrom-Json
    if (
        $probeReport.mediaPackets -lt 1 -or
        $probeReport.continuityErrors -lt 1 -or
        $probeReport.syncRecoveredBytes -lt $garbage.Length
    ) {
        throw "corrupt SRT probe did not recover as expected: $($probeReport | ConvertTo-Json -Compress)"
    }
    return [pscustomobject]@{
        exitCode = $probeExit
        insertedBytes = $garbage.Length
        receivedBytes = $probeReport.bytes
        mediaPackets = $probeReport.mediaPackets
        continuityErrors = $probeReport.continuityErrors
        corruptUnits = $probeReport.corruptUnits
        syncRecoveredBytes = $probeReport.syncRecoveredBytes
    }
}

function Write-ObsConfig {
    param(
        [Parameter(Mandatory)][string]$ScenarioRoot,
        [string]$Destination = ""
    )

    $configRoot = Join-Path $ScenarioRoot "home/.config/obs-studio"
    $null = New-Item -ItemType Directory -Force -Path $configRoot
    $config = @"
[General]
FirstRun=true
ConfirmOnExit=false

[Video]
Renderer=OpenGL

[OBSWebSocket]
FirstLoad=false
ServerEnabled=true
ServerPort=4455
AlertsEnabled=false
AuthRequired=false
ServerPassword=

[BasicWindow]
WarnBeforeStartingStream=false
WarnBeforeStoppingStream=false
ConfirmOnExit=false

[Basic]
Profile=Untitled
ProfileDir=Untitled
SceneCollection=Untitled
SceneCollectionFile=Untitled
ConfigOnNewProfile=false
"@
    [IO.File]::WriteAllText((Join-Path $configRoot "global.ini"), $config, [Text.UTF8Encoding]::new($false))

    if ($Destination) {
        $profileRoot = Join-Path $configRoot "basic/profiles/Untitled"
        $null = New-Item -ItemType Directory -Force -Path $profileRoot
        $profile = @"
[General]
Name=Untitled

[Output]
Mode=Simple
Reconnect=true
RetryDelay=1
MaxRetries=5

[SimpleOutput]
VBitrate=3000
ABitrate=128
Preset=veryfast
StreamAudioEncoder=aac
StreamEncoder=x264

[Video]
BaseCX=1280
BaseCY=720
OutputCX=1280
OutputCY=720
FPSType=0
FPSCommon=30
ColorFormat=NV12
ColorSpace=709
ColorRange=Partial

[Audio]
SampleRate=48000
ChannelSetup=Stereo
"@
        [IO.File]::WriteAllText((Join-Path $profileRoot "basic.ini"), $profile, [Text.UTF8Encoding]::new($false))
        $service = @{
            type = "rtmp_custom"
            settings = @{ server = $Destination; key = "" }
        } | ConvertTo-Json -Compress
        [IO.File]::WriteAllText((Join-Path $profileRoot "service.json"), $service, [Text.UTF8Encoding]::new($false))
    }
}

function ConvertTo-ShellArgument {
    param([Parameter(Mandatory)][string]$Value)
    return "'" + $Value.Replace("'", "'`"'`"'") + "'"
}

function Start-Obs {
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][string]$ScenarioRoot,
        [Parameter(Mandatory)][string[]]$ControllerArguments,
        [string]$Destination = ""
    )

    Write-ObsConfig -ScenarioRoot $ScenarioRoot -Destination $Destination
    $controller = ($ControllerArguments | ForEach-Object { ConvertTo-ShellArgument $_ }) -join " "
    $command = @"
dbus-run-session -- xvfb-run -a -s '-screen 0 1280x720x24 +extension GLX' obs --disable-shutdown-check --multi > /work/obs.log 2>&1 &
obs_pid=`$!
python3 /tools/obs.py $controller > /work/controller.log 2>&1
status=`$?
pkill -INT -x obs 2>/dev/null || true
for attempt in `$(seq 1 50); do
    kill -0 "`$obs_pid" 2>/dev/null || break
    sleep 0.1
done
kill -TERM "`$obs_pid" 2>/dev/null || true
wait "`$obs_pid" 2>/dev/null || true
exit "`$status"
"@
    Start-TestContainer -Name $Name -Arguments @(
        "--gpus", "all",
        "--network", $network,
        "-e", "HOME=/work/home",
        "-e", "LD_LIBRARY_PATH=",
        "-e", "LIBGL_ALWAYS_SOFTWARE=1",
        "-e", "QT_X11_NO_MITSHM=1",
        "-v", "${ScenarioRoot}:/work",
        "-v", "${PSScriptRoot}:/tools:ro",
        $DesktopImage, "sh", "-c", $command
    )
}

function Test-ObsCleanShutdown {
    param([Parameter(Mandatory)][string]$ScenarioRoot)

    $logPath = Join-Path $ScenarioRoot "obs.log"
    if (!(Test-Path $logPath)) {
        throw "OBS did not produce obs.log"
    }
    $log = [IO.File]::ReadAllText($logPath)
    return $log -notmatch "(?i)segmentation fault|core dumped"
}

function Start-Engine {
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][string]$ConfigPath
    )

    Start-TestContainer -Name $Name -Arguments @(
        "--network", $network, "--gpus", "all",
        "-e", "NVIDIA_DRIVER_CAPABILITIES=compute,video,utility",
        "-v", "${ConfigPath}:/config/job.yaml:ro",
        $EngineImage, "run", "-f", "/config/job.yaml"
    )
}

function Stop-Engine {
    param([Parameter(Mandatory)][string]$Name)
    $null = Invoke-Docker -Arguments @("kill", "--signal=SIGINT", $Name) -Capture
    return Wait-ContainerExit -Name $Name -TimeoutSeconds 25
}

function Invoke-ObsReceive {
    $scenarioRoot = Join-Path $runRoot "obs-output"
    $null = New-Item -ItemType Directory -Path $scenarioRoot
    $engine = "am-$runId-obs-output-engine"
    $source = "am-$runId-obs-output-source"
    $obs = "am-$runId-obs-output"
    $configPath = Join-Path $scenarioRoot "job.yaml"
    Write-JobConfig -Path $configPath -InputMode caller -OutputMode caller `
        -InputPort 9001 -OutputPort 10000 -Producer $source -Receiver $obs
    Start-Obs -Name $obs -ScenarioRoot $scenarioRoot -ControllerArguments @(
        "consume", "--source", "srt://0.0.0.0:10000?mode=listener&latency=120000",
        "--screenshot", "/work/frame.png", "--duration", "$DurationSeconds"
    )
    Start-Sleep -Seconds 3
    Start-Producer -Name $source -PeerMode listener -Engine $engine -Port 9001 `
        -Seconds ($DurationSeconds + 20)
    Start-Engine -Name $engine -ConfigPath $configPath
    $state = Wait-ControlState -Engine $engine -TimeoutSeconds 40 -RequireConnected
    $obsExit = Wait-ContainerExit -Name $obs -TimeoutSeconds ($DurationSeconds + 45)
    $obsCleanShutdown = Test-ObsCleanShutdown -ScenarioRoot $scenarioRoot
    $framePath = Join-Path $scenarioRoot "frame.png"
    if (!(Test-Path $framePath) -or (Get-Item $framePath).Length -lt 1000) {
        $logs = Invoke-Docker -Arguments @("logs", "--tail", "120", $obs) -Capture
        throw "OBS did not render the aimedia output:`n$($logs -join [Environment]::NewLine)"
    }
    $engineExit = Stop-Engine -Name $engine
    $null = Invoke-Docker -Arguments @("rm", "-f", $source) -Capture
    $desktopResults.Add([pscustomobject]@{
        scenario = "obs-output"
        role = "consumer"
        obsExit = $obsExit
        cleanShutdown = $obsCleanShutdown
        engineExit = $engineExit
        inputConnected = $state.inputs[0].srt.connected
        outputConnected = $state.output.srt.connected
        screenshotBytes = (Get-Item $framePath).Length
    })
}

function Invoke-ObsSend {
    $scenarioRoot = Join-Path $runRoot "obs-input"
    $null = New-Item -ItemType Directory -Path $scenarioRoot
    Invoke-Docker -Arguments @(
        "run", "--rm", "-v", "${scenarioRoot}:/work", $PeerImage, "ffmpeg",
        "-hide_banner", "-loglevel", "error", "-y",
        "-f", "lavfi", "-i", "smptebars=size=1280x720:rate=30",
        "-f", "lavfi", "-i", "sine=frequency=1500:sample_rate=48000",
        "-t", "$($DurationSeconds + 5)", "-c:v", "libx264", "-preset", "ultrafast",
        "-tune", "zerolatency", "-profile:v", "main", "-pix_fmt", "yuv420p",
        "-bf", "0", "-g", "30", "-c:a", "aac", "-b:a", "128k", "-ar", "48000",
        "-ac", "2", "-f", "mpegts", "/work/source.ts"
    )
    $engine = "am-$runId-obs-input-engine"
    $obs = "am-$runId-obs-input"
    $sink = "am-$runId-obs-input-sink"
    $configPath = Join-Path $scenarioRoot "job.yaml"
    Write-JobConfig -Path $configPath -InputMode listener -OutputMode caller `
        -InputPort 9001 -OutputPort 10000 -Producer $obs -Receiver $sink
    Start-Receiver -Name $sink -PeerMode listener -Engine $engine -Port 10000 `
        -ScenarioRoot $scenarioRoot
    Start-Engine -Name $engine -ConfigPath $configPath
    Start-Sleep -Milliseconds 500
    $destination = "srt://${engine}:9001?mode=caller&latency=120000"
    Start-Obs -Name $obs -ScenarioRoot $scenarioRoot -Destination $destination -ControllerArguments @(
        "produce", "--source", "/work/source.ts",
        "--destination", $destination,
        "--duration", "$DurationSeconds"
    )
    $state = Wait-ControlState -Engine $engine -TimeoutSeconds 40 -RequireConnected
    $obsExit = Wait-ContainerExit -Name $obs -TimeoutSeconds ($DurationSeconds + 45)
    $obsCleanShutdown = Test-ObsCleanShutdown -ScenarioRoot $scenarioRoot
    $engineExit = Stop-Engine -Name $engine
    $sinkExit = Wait-ContainerExit -Name $sink -TimeoutSeconds 20 -AllowFailure
    $probe = Read-OutputProbe -ScenarioRoot $scenarioRoot
    $desktopResults.Add([pscustomobject]@{
        scenario = "obs-input"
        role = "producer"
        obsExit = $obsExit
        cleanShutdown = $obsCleanShutdown
        engineExit = $engineExit
        sinkExit = $sinkExit
        inputConnected = $state.inputs[0].srt.connected
        outputConnected = $state.output.srt.connected
        videoPackets = $probe.videoPackets
        audioPackets = $probe.audioPackets
        firstVideoKeyframe = $probe.firstVideoKeyframe
        monotonicPtsDts = $probe.monotonicPtsDts
    })
}

function Invoke-VlcReceive {
    $scenarioRoot = Join-Path $runRoot "vlc-output"
    $null = New-Item -ItemType Directory -Path $scenarioRoot
    $engine = "am-$runId-vlc-engine"
    $source = "am-$runId-vlc-source"
    $vlc = "am-$runId-vlc-output"
    $configPath = Join-Path $scenarioRoot "job.yaml"
    Write-JobConfig -Path $configPath -InputMode caller -OutputMode listener `
        -InputPort 9001 -OutputPort 10000 -Producer $source -Receiver $vlc
    Start-Producer -Name $source -PeerMode listener -Engine $engine -Port 9001 `
        -Seconds ($DurationSeconds + 15)
    Start-Engine -Name $engine -ConfigPath $configPath
    Start-Sleep -Milliseconds 500
    $vlcCommand = @"
rm -f /work/vlc.ts
timeout --signal=INT ${DurationSeconds}s \
    cvlc -I dummy --no-video-title-show --play-and-exit \
    --demux=dump --demuxdump-file=/work/vlc.ts \
    'srt://${engine}:10000' || true
test -s /work/vlc.ts
"@
    Start-TestContainer -Name $vlc -Arguments @(
        "--network", $network, "--user", "911:1001", "-e", "HOME=/tmp", "-e", "LD_LIBRARY_PATH=",
        "-v", "${scenarioRoot}:/work", $DesktopImage,
        "sh", "-c", $vlcCommand
    )
    $state = Wait-ControlState -Engine $engine -TimeoutSeconds 40 -RequireConnected
    $vlcExit = Wait-ContainerExit -Name $vlc -TimeoutSeconds ($DurationSeconds + 30)
    $engineExit = Stop-Engine -Name $engine
    $null = Invoke-Docker -Arguments @("rm", "-f", $source) -Capture
    $probe = Read-OutputProbe -ScenarioRoot $scenarioRoot -FileName "vlc.ts"
    $desktopResults.Add([pscustomobject]@{
        scenario = "vlc-output"
        role = "consumer"
        vlcExit = $vlcExit
        engineExit = $engineExit
        inputConnected = $state.inputs[0].srt.connected
        outputConnected = $state.output.srt.connected
        videoPackets = $probe.videoPackets
        audioPackets = $probe.audioPackets
        firstVideoKeyframe = $probe.firstVideoKeyframe
        monotonicPtsDts = $probe.monotonicPtsDts
    })
}

try {
    $null = New-Item -ItemType Directory -Path $runRoot
    $null = Invoke-Docker -Arguments @("network", "create", $network) -Capture
    if (!$SkipToolBuild) {
        $buildArguments = @(
            "build", "-f", (Join-Path $repoRoot "docker/Dockerfile.test"),
            "--target", "network", "-t", $PeerImage
        )
        if ($AptMirror) {
            $buildArguments += @("--build-arg", "APT_MIRROR=$AptMirror")
        }
        $buildArguments += $repoRoot
        Invoke-Docker -Arguments $buildArguments
        if ($needsDesktop) {
            $desktopBuildArguments = @(
                "build", "-f", (Join-Path $repoRoot "docker/Dockerfile.test"),
                "--target", "desktop", "-t", $DesktopImage
            )
            if ($AptMirror) {
                $desktopBuildArguments += @("--build-arg", "APT_MIRROR=$AptMirror")
            }
            $desktopBuildArguments += $repoRoot
            Invoke-Docker -Arguments $desktopBuildArguments
        }
    } else {
        $null = Invoke-Docker -Arguments @("image", "inspect", $PeerImage) -Capture
        if ($needsDesktop) {
            $null = Invoke-Docker -Arguments @("image", "inspect", $DesktopImage) -Capture
        }
    }
    $null = Invoke-Docker -Arguments @("image", "inspect", $EngineImage) -Capture
    $toolVersions.ffmpeg = (Invoke-Docker -Arguments @(
        "run", "--rm", $PeerImage, "ffmpeg", "-version"
    ) -Capture | Select-Object -First 1)
    if ($needsDesktop) {
        $null = Invoke-Docker -Arguments @(
            "run", "--rm", "-e", "LD_LIBRARY_PATH=", $DesktopImage,
            "sh", "-c", "test -f /usr/lib/x86_64-linux-gnu/vlc/plugins/access/libaccess_srt_plugin.so"
        ) -Capture
        $toolVersions.obs = (Invoke-Docker -Arguments @(
            "run", "--rm", $DesktopImage, "obs", "--version"
        ) -Capture | Select-Object -First 1)
        $toolVersions.vlc = (Invoke-Docker -Arguments @(
            "run", "--rm", "--user", "911:1001", "-e", "HOME=/tmp",
            $DesktopImage, "cvlc", "--version"
        ) -Capture | Select-Object -First 1)
    }

    if ($Suite -in @("all", "matrix", "netem")) {
        Invoke-Scenario -Name "ll-netem" -InputMode listener -OutputMode listener -Impaired
    }
    if ($Suite -eq "ll") {
        Invoke-Scenario -Name "ll" -InputMode listener -OutputMode listener
    }
    if ($Suite -in @("all", "matrix", "lc")) {
        Invoke-Scenario -Name "lc" -InputMode listener -OutputMode caller
    }
    if ($Suite -in @("all", "matrix", "cl")) {
        Invoke-Scenario -Name "cl" -InputMode caller -OutputMode listener
    }
    if ($Suite -in @("all", "matrix", "cc")) {
        Invoke-Scenario -Name "cc" -InputMode caller -OutputMode caller
    }
    if ($Suite -in @("all", "corrupt")) {
        $corruptResult = Invoke-CorruptProbe
    }
    if ($Suite -in @("all", "desktop", "obs", "obs-input")) {
        Invoke-ObsSend
    }
    if ($Suite -in @("all", "desktop", "obs", "obs-output")) {
        Invoke-ObsReceive
    }
    if ($Suite -in @("all", "desktop", "vlc")) {
        Invoke-VlcReceive
    }

    $report = [pscustomobject]@{
        schema = "aimedia.interop/v1alpha1"
        createdAt = [DateTimeOffset]::UtcNow.ToString("O")
        engineImage = $EngineImage
        peerImage = $PeerImage
        desktopImage = $DesktopImage
        durationSeconds = $DurationSeconds
        suite = $Suite
        toolVersions = $toolVersions
        scenarios = $results
        corruptProbe = $corruptResult
        desktop = $desktopResults
    }
    $reportPath = Join-Path $runRoot "summary.json"
    [IO.File]::WriteAllText(
        $reportPath,
        ($report | ConvertTo-Json -Depth 8),
        [Text.UTF8Encoding]::new($false)
    )
    Write-Host "interop report: $reportPath"
    $report | ConvertTo-Json -Depth 8
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
