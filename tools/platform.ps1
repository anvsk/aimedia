param(
    [string]$EngineImage = "aimedia:gpu",
    [string]$PeerImage = "aimedia:test-tools",
    [Parameter(Mandatory)]
    [ValidateSet("youtube", "twitch", "tencent", "aliyun", "custom")]
    [string]$Platform,
    [Parameter(Mandatory)]
    [string]$Endpoint,
    [string]$StreamNameEnv = "AIMEDIA_PLATFORM_STREAM_NAME",
    [string]$PublishQueryEnv = "",
    [ValidateRange(30, 900)]
    [int]$DurationSeconds = 60,
    [switch]$HandshakeOnly,
    [switch]$ExpectPublishReject,
    [switch]$KeepContainers
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Assert-EnvironmentName {
    param([Parameter(Mandatory)][string]$Name)
    if ($Name -notmatch '^[A-Za-z_][A-Za-z0-9_]*$') {
        throw "secret reference names must use portable environment-variable syntax"
    }
}

Assert-EnvironmentName -Name $StreamNameEnv
$streamNameSecret = [Environment]::GetEnvironmentVariable($StreamNameEnv)
if ([string]::IsNullOrEmpty($streamNameSecret)) {
    throw "stream name secret environment variable is not set"
}

$querySecret = ""
if ($PublishQueryEnv) {
    Assert-EnvironmentName -Name $PublishQueryEnv
    $querySecret = [Environment]::GetEnvironmentVariable($PublishQueryEnv)
    if ([string]::IsNullOrEmpty($querySecret)) {
        throw "publish query secret environment variable is not set"
    }
}

if ($Endpoint -notmatch '^rtmps?://[A-Za-z0-9.-]+(?::[0-9]{1,5})?/[A-Za-z0-9._~-]+(?:/[A-Za-z0-9._~-]+)*$') {
    throw "Endpoint must be an RTMP/RTMPS base URI without credentials, stream name, query, fragment, or trailing slash"
}
$tls = $Endpoint.StartsWith("rtmps://", [StringComparison]::OrdinalIgnoreCase)
if ($Platform -eq "youtube" -and !$tls) {
    throw "the YouTube gate requires the RTMPS URL copied from Live Control Room"
}
if ($Platform -eq "youtube" -and $PublishQueryEnv) {
    throw "the YouTube preset keeps its stream key in StreamNameEnv and does not use PublishQueryEnv"
}
if ($Platform -eq "twitch" -and (!$PublishQueryEnv -or $querySecret -notmatch '(^|&)bandwidthtest=true(&|$)')) {
    throw "the Twitch gate requires bandwidthtest=true through PublishQueryEnv"
}
if ($Platform -eq "tencent" -and (!$PublishQueryEnv -or $querySecret -notmatch '(^|&)txSecret=' -or $querySecret -notmatch '(^|&)txTime=')) {
    throw "the Tencent gate requires txSecret and txTime through PublishQueryEnv"
}
if ($Platform -eq "aliyun" -and (!$PublishQueryEnv -or $querySecret -notmatch '(^|&)auth_key=')) {
    throw "the Alibaba Cloud gate requires auth_key through PublishQueryEnv"
}

$runId = [Guid]::NewGuid().ToString("N").Substring(0, 8)
$network = "am-platform-$runId"
$engine = "am-$runId-engine"
$source = "am-$runId-source"
$runRoot = Join-Path ([IO.Path]::GetTempPath()) "aimedia-platform-$runId"
$configPath = Join-Path $runRoot "job.yaml"
$summaryPath = Join-Path $runRoot "summary.json"
$containers = @($source, $engine)
$gateMode = if ($HandshakeOnly -or $ExpectPublishReject) { "handshake" } else { "media" }
$expectedObservation = if ($ExpectPublishReject) { "publishRejected" } else { "accepted" }

function Invoke-Docker {
    param(
        [Parameter(Mandatory)][string[]]$Arguments,
        [switch]$Capture,
        [switch]$AllowFailure
    )
    $output = & docker @Arguments 2>&1
    $exitCode = $LASTEXITCODE
    if (!$AllowFailure -and $exitCode -ne 0) {
        throw "docker command failed with exit code $exitCode"
    }
    if ($Capture) {
        return $output
    }
}

function Get-RedactedEngineLogs {
    $lines = Invoke-Docker -Arguments @("logs", "--tail", "160", $engine) -Capture -AllowFailure
    $text = ($lines -join [Environment]::NewLine)
    if ($streamNameSecret) {
        $text = $text.Replace($streamNameSecret, "<redacted>")
    }
    if ($querySecret) {
        $text = $text.Replace($querySecret, "<redacted>")
    }
    return $text
}

function Get-State {
    $text = & docker exec $engine aimedia control state --json 2>$null
    $exitCode = $LASTEXITCODE
    if ($exitCode -ne 0) {
        return $null
    }
    try {
        return (($text -join [Environment]::NewLine) | ConvertFrom-Json).state
    }
    catch {
        return $null
    }
}

function Wait-SuccessState {
    param([int]$TimeoutSeconds = 75)
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        $state = Get-State
        if (
            $null -ne $state -and
            $state.running -and
            $state.inputs[0].srt.connected -and
            $state.output.rtmp.connected -and
            $state.output.rtmp.packetsSent -gt 0
        ) {
            return $state
        }
        $running = Invoke-Docker -Arguments @("inspect", "-f", "{{.State.Running}}", $engine) -Capture -AllowFailure
        if (($running -join "").Trim() -eq "false") {
            throw "platform publisher exited before becoming ready:`n$(Get-RedactedEngineLogs)"
        }
        Start-Sleep -Milliseconds 500
    } while ((Get-Date) -lt $deadline)
    throw "platform publisher did not become ready:`n$(Get-RedactedEngineLogs)"
}

function Invoke-PublishCheck {
    param([Parameter(Mandatory)][string[]]$SecretArguments)
    $arguments = @(
        "run", "--name", $engine, "-e", "RUST_LOG=off"
    ) + $SecretArguments + @(
        "-v", "${configPath}:/config/job.yaml:ro",
        $EngineImage, "publish-check", "-f", "/config/job.yaml", "--json"
    )
    $output = & docker @arguments 2>&1
    $exitCode = $LASTEXITCODE
    $raw = ($output -join [Environment]::NewLine)
    if ($raw.Contains($streamNameSecret) -or ($querySecret -and $raw.Contains($querySecret))) {
        throw "publish-check output exposed a platform credential"
    }
    return [pscustomobject]@{
        exitCode = $exitCode
        output = $raw
    }
}

$queryYaml = ""
if ($PublishQueryEnv) {
    $queryYaml = @"
      publishQueryRef:
        env: $PublishQueryEnv
"@
}
$config = @"
apiVersion: aimedia/v1alpha2
kind: MediaJob
metadata:
  name: platform-$Platform
inputs:
  - name: contribution
    role: custom
    uri: srt://0.0.0.0:9000
    srt:
      mode: listener
      latencyMs: 120
processing:
  video:
    width: 1920
    height: 1080
    fps: 30
    bitrateKbps: 6000
    gopMs: 2000
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
    uri: $Endpoint
    rtmp:
      mode: publish
      streamNameRef:
        env: $StreamNameEnv
$queryYaml
      connectTimeoutMs: 5000
      handshakeTimeoutMs: 10000
      readTimeoutMs: 10000
      maxMessageBytes: 8388608
      reconnect:
        enabled: true
        initialBackoffMs: 500
        maxBackoffMs: 5000
taps: []
control:
  socketPath: /run/aimedia/aimedia.sock
  socketMode: "0660"
"@

$summary = $null
$engineDigest = $null
$peerDigest = $null
try {
    $null = New-Item -ItemType Directory -Path $runRoot
    [IO.File]::WriteAllText($configPath, $config, [Text.UTF8Encoding]::new($false))
    $null = Invoke-Docker -Arguments @("image", "inspect", $EngineImage) -Capture
    $engineDigest = (Invoke-Docker -Arguments @("image", "inspect", $EngineImage, "--format", "{{.Id}}") -Capture).Trim()
    if ($gateMode -eq "media") {
        $null = Invoke-Docker -Arguments @("image", "inspect", $PeerImage) -Capture
        $peerDigest = (Invoke-Docker -Arguments @("image", "inspect", $PeerImage, "--format", "{{.Id}}") -Capture).Trim()
    }
    $secretArgs = @("-e", $StreamNameEnv)
    if ($PublishQueryEnv) {
        $secretArgs += @("-e", $PublishQueryEnv)
    }
    if ($HandshakeOnly -or $ExpectPublishReject) {
        $check = Invoke-PublishCheck -SecretArguments $secretArgs
        if ($ExpectPublishReject) {
            if ($check.exitCode -eq 0 -or $check.output -notmatch 'PublishRejected during Command') {
                throw "platform did not return the stable publish-rejection path:`n$($check.output)"
            }
            $result = "publishRejected"
        }
        else {
            if ($check.exitCode -ne 0) {
                throw "platform publish check failed:`n$($check.output)"
            }
            $accepted = $check.output | ConvertFrom-Json
            if ($accepted.accepted -ne $true) {
                throw "platform publish check did not report an accepted session"
            }
            $result = "accepted"
        }
        $summary = [ordered]@{
            schema = "aimedia.platform/v1alpha1"
            createdAt = [DateTimeOffset]::UtcNow.ToString("o")
            platform = $Platform
            endpoint = $Endpoint
            tls = $tls
            mode = $gateMode
            expected = $expectedObservation
            result = "passed"
            observed = $result
            engineExit = $check.exitCode
            secretsRedacted = $true
            engineImage = $engineDigest
            peerImage = $peerDigest
        }
        $json = $summary | ConvertTo-Json -Depth 8
        [IO.File]::WriteAllText($summaryPath, $json, [Text.UTF8Encoding]::new($false))
        Write-Host "platform report: $summaryPath"
        Write-Output $json
        return
    }

    $null = Invoke-Docker -Arguments @("network", "create", $network) -Capture
    $engineArgs = @(
        "run", "-d", "--name", $engine, "--network", $network, "--gpus", "all",
        "-e", "NVIDIA_DRIVER_CAPABILITIES=compute,video,utility"
    ) + $secretArgs + @(
        "-v", "${configPath}:/config/job.yaml:ro",
        $EngineImage, "run", "-f", "/config/job.yaml"
    )
    $null = Invoke-Docker -Arguments $engineArgs -Capture

    $null = Invoke-Docker -Arguments @(
        "run", "-d", "--name", $source, "--network", $network,
        $PeerImage, "ffmpeg", "-hide_banner", "-loglevel", "warning", "-re",
        "-f", "lavfi", "-i", "testsrc2=size=1920x1080:rate=30",
        "-f", "lavfi", "-i", "sine=frequency=1200:sample_rate=48000",
        "-t", "$($DurationSeconds + 90)",
        "-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency",
        "-profile:v", "main", "-pix_fmt", "yuv420p", "-bf", "0",
        "-g", "60", "-keyint_min", "60", "-sc_threshold", "0", "-b:v", "6000k",
        "-c:a", "aac", "-b:a", "128k", "-ar", "48000", "-ac", "2",
        "-f", "mpegts", "srt://${engine}:9000?mode=caller&latency=120000"
    ) -Capture

    $initial = Wait-SuccessState
    $timer = [Diagnostics.Stopwatch]::StartNew()
    $minimumPackets = [int64]$initial.output.rtmp.packetsSent
    while ($timer.Elapsed.TotalSeconds -lt $DurationSeconds) {
        Start-Sleep -Seconds 1
        $state = Get-State
        if ($null -eq $state -or !$state.running -or !$state.output.rtmp.connected) {
            throw "platform publisher lost its accepted session:`n$(Get-RedactedEngineLogs)"
        }
        $minimumPackets = [Math]::Min($minimumPackets, [int64]$state.output.rtmp.packetsSent)
    }
    $final = Get-State
    if ($null -eq $final -or [int64]$final.output.rtmp.packetsSent -le $minimumPackets) {
        throw "platform accepted the session but media packet count did not advance"
    }
    if ([int64]$final.output.rtmp.reconnects -ne 0) {
        throw "platform session reconnected unexpectedly"
    }
    $rawLogs = (Invoke-Docker -Arguments @("logs", $engine) -Capture -AllowFailure) -join [Environment]::NewLine
    if ($rawLogs.Contains($streamNameSecret) -or ($querySecret -and $rawLogs.Contains($querySecret))) {
        throw "engine logs exposed a platform credential"
    }
    $summary = [ordered]@{
        schema = "aimedia.platform/v1alpha1"
        createdAt = [DateTimeOffset]::UtcNow.ToString("o")
        platform = $Platform
        endpoint = $Endpoint
        tls = $tls
        mode = "media"
        expected = "accepted"
        result = "passed"
        durationSeconds = $DurationSeconds
        inputPackets = [int64]$final.inputs[0].srt.packetsReceived
        outputPackets = [int64]$final.output.rtmp.packetsSent
        outputReconnects = [int64]$final.output.rtmp.reconnects
        secretsRedacted = $true
        engineImage = $engineDigest
        peerImage = $peerDigest
    }
    $json = $summary | ConvertTo-Json -Depth 8
    [IO.File]::WriteAllText($summaryPath, $json, [Text.UTF8Encoding]::new($false))
    Write-Host "platform report: $summaryPath"
    Write-Output $json
}
catch {
    $failure = $_.Exception.Message
    if ($streamNameSecret) {
        $failure = $failure.Replace($streamNameSecret, "<redacted>")
    }
    if ($querySecret) {
        $failure = $failure.Replace($querySecret, "<redacted>")
    }
    $failureCode = $null
    $failureStage = $null
    if ($failure -match '(?<code>[A-Za-z][A-Za-z0-9]*) during (?<stage>[A-Za-z][A-Za-z0-9]*):') {
        $failureCode = $Matches.code
        $failureStage = $Matches.stage
    }
    $summary = [ordered]@{
        schema = "aimedia.platform/v1alpha1"
        createdAt = [DateTimeOffset]::UtcNow.ToString("o")
        platform = $Platform
        endpoint = $Endpoint
        tls = $tls
        mode = $gateMode
        expected = $expectedObservation
        result = "failed"
        failureCode = $failureCode
        failureStage = $failureStage
        message = $failure
        secretsRedacted = $true
        engineImage = $engineDigest
        peerImage = $peerDigest
    }
    if (Test-Path -LiteralPath $runRoot) {
        $json = $summary | ConvertTo-Json -Depth 8
        [IO.File]::WriteAllText($summaryPath, $json, [Text.UTF8Encoding]::new($false))
        Write-Host "platform report: $summaryPath"
        Write-Output $json
    }
    throw "platform gate failed; report: $summaryPath`n$failure"
}
finally {
    if (!$KeepContainers) {
        foreach ($name in $containers) {
            $null = Invoke-Docker -Arguments @("rm", "-f", $name) -Capture -AllowFailure
        }
        $null = Invoke-Docker -Arguments @("network", "rm", $network) -Capture -AllowFailure
    }
}
