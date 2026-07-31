use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use aimedia_aac::Libxaac;
use aimedia_core::{
    CameraSnapshot, ControlRequest, ControlResponse, Director, PipelineConfig,
    backend::Transport,
    config::{SrtConfig, SrtMode},
    vlm::VlmAdvice,
};
use aimedia_mpegts::{DemuxEvent, ProgramMap, StreamDemuxer, probe_path};
use aimedia_nvidia::NvidiaLibraries;
use aimedia_runtime::{run_mock_pipeline, send_control_request};
use aimedia_srt::{Endpoint, SrtTransport, probe_version as probe_srt_version};
use anyhow::{Context, Result, bail};
use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "aimedia",
    version,
    about = "AI-native dual-camera live director"
)]
struct Cli {
    #[arg(short, long, action = ArgAction::Count, global = true)]
    verbose: u8,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Probe an MPEG-TS capture or sample a live SRT/MPEG-TS input.
    Probe {
        source: String,
        #[arg(long)]
        json: bool,
        /// SRT connection mode. URI mode= remains supported when this is omitted.
        #[arg(long)]
        mode: Option<SrtCliMode>,
        /// How long a live SRT probe collects media.
        #[arg(long, default_value_t = 3_000)]
        duration_ms: u64,
    },
    /// Validate and run a director pipeline.
    Run {
        #[arg(short = 'f', long)]
        file: PathBuf,
        /// Validate the complete graph without opening transports or hardware.
        #[arg(long)]
        dry_run: bool,
        /// Run the real scheduler/control plane with synthetic healthy inputs and no media I/O.
        #[arg(long)]
        mock: bool,
    },
    /// Explain graph topology, memory domains, and real-time policies.
    Explain {
        #[arg(short = 'f', long)]
        file: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Replay timestamped analyzer and VLM events through the real director state machine.
    Replay {
        capture: PathBuf,
        #[arg(short = 'f', long)]
        file: PathBuf,
    },
    /// Benchmark the deterministic director using a replay capture.
    Bench {
        #[arg(short = 'f', long)]
        file: PathBuf,
        #[arg(long)]
        capture: PathBuf,
        #[arg(long, default_value_t = 1_000)]
        iterations: u32,
    },
    /// Control a running local pipeline through its Unix domain socket.
    Control {
        #[arg(long, default_value = "/run/aimedia/aimedia.sock")]
        socket: PathBuf,
        #[command(subcommand)]
        action: ControlAction,
    },
    /// Check native libraries and the GPU without starting a pipeline.
    Doctor {
        #[arg(long)]
        json: bool,
        /// Return a non-zero status unless SRT, NVIDIA, and AAC libraries are all ready.
        #[arg(long)]
        strict: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ControlAction {
    /// Switch to an eligible input. A zero hold keeps manual mode until `auto`.
    Take {
        #[arg(long)]
        input: String,
        #[arg(long, default_value_t = 5_000)]
        hold_ms: u64,
    },
    /// Return control to the automatic director.
    Auto,
    /// Read current camera, health, synchronization, and transport state.
    State {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SrtCliMode {
    Caller,
    Listener,
}

impl From<SrtCliMode> for SrtMode {
    fn from(value: SrtCliMode) -> Self {
        match value {
            SrtCliMode::Caller => Self::Caller,
            SrtCliMode::Listener => Self::Listener,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    match cli.command {
        Command::Probe {
            source,
            json,
            mode,
            duration_ms,
        } => command_probe(&source, json, mode, duration_ms).await,
        Command::Run {
            file,
            dry_run,
            mock,
        } => command_run(&file, dry_run, mock).await,
        Command::Explain { file, json } => command_explain(&file, json),
        Command::Replay { capture, file } => command_replay(&file, &capture),
        Command::Bench {
            file,
            capture,
            iterations,
        } => command_bench(&file, &capture, iterations),
        Command::Control { socket, action } => command_control(&socket, action).await,
        Command::Doctor { json, strict } => command_doctor(json, strict),
    }
}

fn command_doctor(output_json: bool, strict: bool) -> Result<()> {
    let srt = match probe_srt_version() {
        Ok(version) => json!({
            "ready": true,
            "versionRaw": format!("0x{version:06x}"),
        }),
        Err(error) => json!({"ready": false, "error": error.to_string()}),
    };
    let nvidia = match NvidiaLibraries::load() {
        Ok(libraries) => json!({"ready": true, "details": libraries.report()}),
        Err(error) => json!({"ready": false, "error": error.to_string()}),
    };
    let aac = match Libxaac::load() {
        Ok(library) => json!({"ready": true, "details": library.report()}),
        Err(error) => json!({"ready": false, "error": error.to_string()}),
    };
    let ready = [&srt, &nvidia, &aac]
        .iter()
        .all(|component| component["ready"].as_bool() == Some(true));
    let report = json!({
        "ready": ready,
        "runtimeDependencyOnFfmpeg": false,
        "components": {
            "srt": srt,
            "nvidia": nvidia,
            "aac": aac,
        }
    });
    if output_json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("native backend ready: {ready}");
        for name in ["srt", "nvidia", "aac"] {
            let component = &report["components"][name];
            if component["ready"].as_bool() == Some(true) {
                println!("  {name:<8} ready");
            } else {
                println!(
                    "  {name:<8} unavailable: {}",
                    component["error"].as_str().unwrap_or("unknown error")
                );
            }
        }
    }
    if strict && !ready {
        bail!("one or more native backend dependencies are unavailable");
    }
    Ok(())
}

fn init_tracing(verbose: u8) {
    let default_filter = match verbose {
        0 => "aimedia=info",
        1 => "aimedia=debug",
        _ => "aimedia=trace",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter)),
        )
        .with_target(false)
        .compact()
        .init();
}

async fn command_probe(
    source: &str,
    output_json: bool,
    mode: Option<SrtCliMode>,
    duration_ms: u64,
) -> Result<()> {
    if source.starts_with("srt://") {
        return command_probe_srt(source, output_json, mode, duration_ms).await;
    }
    let path = source.strip_prefix("file://").unwrap_or(source);
    let report = probe_path(path).with_context(|| format!("failed to probe {path:?}"))?;
    if output_json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "MPEG-TS: {} packets, sync offset {}, trailing {} bytes",
            report.packets, report.sync_offset, report.trailing_bytes
        );
        for program in &report.programs {
            println!(
                "program {}: PMT PID 0x{:04x}, PCR PID {}",
                program.program_number,
                program.pmt_pid,
                program
                    .pcr_pid
                    .map_or_else(|| "unknown".to_owned(), |pid| format!("0x{pid:04x}"))
            );
            for stream in &program.streams {
                println!(
                    "  PID 0x{:04x}: {} (stream type 0x{:02x})",
                    stream.pid, stream.codec, stream.stream_type
                );
            }
        }
        for pid in &report.pids {
            if pid.continuity_errors > 0 || pid.transport_errors > 0 {
                println!(
                    "warning PID 0x{:04x}: {} continuity errors, {} transport errors",
                    pid.pid, pid.continuity_errors, pid.transport_errors
                );
            }
        }
    }
    Ok(())
}

async fn command_probe_srt(
    source: &str,
    output_json: bool,
    mode: Option<SrtCliMode>,
    duration_ms: u64,
) -> Result<()> {
    if duration_ms == 0 || duration_ms > 60_000 {
        bail!("--duration-ms must be between 1 and 60000");
    }
    let config = SrtConfig {
        mode: mode.map(Into::into),
        ..SrtConfig::default()
    };
    let endpoint = Endpoint::from_config(source, &config, None)?;
    let mut transport = SrtTransport::connect(endpoint).await.with_context(|| {
        format!(
            "failed to connect SRT probe to {:?}",
            redact_srt_uri(source)
        )
    })?;
    let started = Instant::now();
    let deadline = Duration::from_millis(duration_ms);
    let mut demuxer = StreamDemuxer::new();
    let mut bytes = 0_u64;
    let mut media_packets = 0_u64;
    let mut continuity_errors = 0_u64;
    let mut discontinuities = 0_u64;
    let mut corrupt_units = 0_u64;
    let mut sync_recovered_bytes = 0_u64;
    let mut program_map: Option<ProgramMap> = None;

    while started.elapsed() < deadline {
        let payload = transport.receive().await?;
        bytes = bytes.saturating_add(payload.len() as u64);
        for event in demuxer.push(&payload)? {
            match event {
                DemuxEvent::ProgramMap(map) => program_map = Some(map),
                DemuxEvent::Packet(_) => media_packets = media_packets.saturating_add(1),
                DemuxEvent::ContinuityError { .. } => {
                    continuity_errors = continuity_errors.saturating_add(1);
                }
                DemuxEvent::Discontinuity { .. } => {
                    discontinuities = discontinuities.saturating_add(1);
                }
                DemuxEvent::SyncRecovered { discarded_bytes } => {
                    sync_recovered_bytes =
                        sync_recovered_bytes.saturating_add(discarded_bytes as u64);
                }
                DemuxEvent::CorruptData { .. } => {
                    corrupt_units = corrupt_units.saturating_add(1);
                }
            }
        }
    }
    for event in demuxer.flush()? {
        if matches!(event, DemuxEvent::Packet(_)) {
            media_packets = media_packets.saturating_add(1);
        }
    }
    let stats = transport.stats().await?;
    transport.close().await?;
    let report = json!({
        "source": redact_srt_uri(source),
        "durationMs": started.elapsed().as_millis(),
        "bytes": bytes,
        "mediaPackets": media_packets,
        "continuityErrors": continuity_errors,
        "discontinuities": discontinuities,
        "corruptUnits": corrupt_units,
        "syncRecoveredBytes": sync_recovered_bytes,
        "program": program_map,
        "srt": stats,
    });
    if output_json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "SRT/MPEG-TS: {bytes} bytes, {media_packets} media packets, \
             {continuity_errors} continuity errors"
        );
        if let Some(program) = report["program"].as_object() {
            println!(
                "program {}, video PID {}, audio PID {}",
                program["programNumber"], program["videoPid"], program["audioPid"]
            );
        }
    }
    Ok(())
}

fn redact_srt_uri(uri: &str) -> String {
    let Some((base, query)) = uri.split_once('?') else {
        return redact_uri_userinfo(uri);
    };
    let safe = query
        .split('&')
        .filter(|pair| {
            let lower = pair.to_ascii_lowercase();
            !["passphrase=", "password=", "token=", "secret=", "streamid="]
                .iter()
                .any(|sensitive| lower.starts_with(sensitive))
        })
        .collect::<Vec<_>>();
    if safe.is_empty() {
        redact_uri_userinfo(base)
    } else {
        format!("{}?{}", redact_uri_userinfo(base), safe.join("&"))
    }
}

fn redact_uri_userinfo(uri: &str) -> String {
    let Some(authority) = uri.strip_prefix("srt://") else {
        return uri.to_owned();
    };
    let authority_end = authority.find('/').unwrap_or(authority.len());
    let (host, suffix) = authority.split_at(authority_end);
    match host.rsplit_once('@') {
        Some((_, public_host)) => format!("srt://<redacted>@{public_host}{suffix}"),
        None => uri.to_owned(),
    }
}

async fn command_run(path: &Path, dry_run: bool, mock: bool) -> Result<()> {
    let config = load_config(path)?;
    let graph = explain_graph(&config);
    if dry_run {
        println!("{}", serde_json::to_string_pretty(&graph)?);
        info!(pipeline = %config.metadata.name, "configuration and graph are valid");
        return Ok(());
    }
    if mock {
        info!(
            pipeline = %config.metadata.name,
            socket = %config.control.socket_path.display(),
            "starting mock scheduler and control plane; media I/O is disabled"
        );
        return run_mock_pipeline(config)
            .await
            .context("mock pipeline failed");
    }
    bail!(
        "native live execution is not complete in this build: the streaming MPEG-TS and libsrt \
         layers are present, but NVDEC/NVENC and libxaac frame processing are not linked into the \
         scheduler; use `aimedia run --mock` to exercise the program clock and control socket, or \
         `aimedia run --dry-run` to validate the graph"
    )
}

async fn command_control(socket: &Path, action: ControlAction) -> Result<()> {
    let request_id = request_id();
    let output_json = matches!(&action, ControlAction::State { json: true });
    let request = match action {
        ControlAction::Take { input, hold_ms } => ControlRequest::take(request_id, input, hold_ms),
        ControlAction::Auto => ControlRequest::auto(request_id),
        ControlAction::State { .. } => ControlRequest::state(request_id),
    };
    let response = send_control_request(socket, &request)
        .await
        .with_context(|| format!("failed to contact control socket {}", socket.display()))?;
    print_control_response(&response, output_json)?;
    if !response.accepted {
        bail!(
            "control request rejected: {}",
            response
                .message
                .as_deref()
                .unwrap_or("no rejection reason was provided")
        );
    }
    Ok(())
}

fn print_control_response(response: &ControlResponse, output_json: bool) -> Result<()> {
    if output_json {
        println!("{}", serde_json::to_string_pretty(response)?);
        return Ok(());
    }
    if let Some(state) = &response.state {
        println!(
            "{}: active={}, mode={:?}, reason={}",
            if response.accepted {
                "accepted"
            } else {
                "rejected"
            },
            state.active_name,
            state.mode,
            state.last_reason
        );
    } else if let Some(message) = &response.message {
        println!("rejected: {message}");
    }
    Ok(())
}

fn request_id() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("cli-{}-{timestamp}", std::process::id())
}

fn command_explain(path: &Path, output_json: bool) -> Result<()> {
    let config = load_config(path)?;
    let graph = explain_graph(&config);
    if output_json {
        println!("{}", serde_json::to_string_pretty(&graph)?);
    } else {
        println!("pipeline: {}", config.metadata.name);
        println!("profile: 2x SRT/MPEG-TS H.264+AAC -> director -> 1x SRT/MPEG-TS H.264+AAC");
        println!(
            "sync: master input {}, buffer {}ms, max skew {}ms",
            config.sync.master_input, config.sync.buffer_ms, config.sync.max_skew_ms
        );
        println!(
            "director: min shot {}ms, margin {:.2}, candidate hold {}ms, cooldown {}ms",
            config.director_policy.min_shot_ms,
            config.director_policy.score_margin,
            config.director_policy.candidate_hold_ms,
            config.director_policy.cooldown_ms
        );
        println!(
            "VLM: {:?}, weight {:.2}, deadline {}ms; never on the media hot path",
            config.vlm_advisor.mode, config.vlm_advisor.weight, config.vlm_advisor.deadline_ms
        );
        println!(
            "audio: {}ms equal-power switch, target {:.1} LUFS, peak {:.1} dBFS",
            config.audio_switch.crossfade_ms,
            config.audio_switch.target_lufs,
            config.audio_switch.true_peak_dbfs
        );
        println!("graph:");
        for node in graph["nodes"].as_array().into_iter().flatten() {
            println!(
                "  {:<18} {:<14} {}",
                node["id"].as_str().unwrap_or("?"),
                node["memory"].as_str().unwrap_or("?"),
                node["description"].as_str().unwrap_or("?")
            );
        }
    }
    Ok(())
}

fn command_replay(config_path: &Path, capture_path: &Path) -> Result<()> {
    let config = load_config(config_path)?;
    let records = load_capture(capture_path)?;
    let result = execute_replay(&config, &records, true)?;
    info!(
        decisions = result.decisions,
        switches = result.switches,
        "replay complete"
    );
    Ok(())
}

fn command_bench(config_path: &Path, capture_path: &Path, iterations: u32) -> Result<()> {
    if iterations == 0 {
        bail!("--iterations must be greater than zero");
    }
    let config = load_config(config_path)?;
    let records = load_capture(capture_path)?;
    let started = Instant::now();
    let mut decisions = 0_u64;
    let mut switches = 0_u64;
    for _ in 0..iterations {
        let result = execute_replay(&config, &records, false)?;
        decisions += result.decisions;
        switches += result.switches;
    }
    let elapsed = started.elapsed();
    let nanos_per_decision = if decisions == 0 {
        0
    } else {
        elapsed.as_nanos() / u128::from(decisions)
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "iterations": iterations,
            "recordsPerIteration": records.len(),
            "decisions": decisions,
            "switches": switches,
            "elapsedMs": elapsed.as_secs_f64() * 1000.0,
            "nanosPerDecision": nanos_per_decision,
        }))?
    );
    Ok(())
}

fn load_config(path: &Path) -> Result<PipelineConfig> {
    PipelineConfig::from_yaml_file(path)
        .with_context(|| format!("invalid pipeline configuration {}", path.display()))
}

fn load_capture(path: &Path) -> Result<Vec<ReplayRecord>> {
    let contents =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut records: Vec<ReplayRecord> = Vec::new();
    for (line_index, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let record: ReplayRecord = serde_json::from_str(line).with_context(|| {
            format!(
                "invalid replay record at {}:{}",
                path.display(),
                line_index + 1
            )
        })?;
        if let Some(previous) = records.last() {
            if record.at_ms < previous.at_ms {
                bail!(
                    "replay timestamps must be monotonic at {}:{}",
                    path.display(),
                    line_index + 1
                );
            }
        }
        records.push(record);
    }
    if records.is_empty() {
        bail!("replay capture contains no records");
    }
    Ok(records)
}

fn execute_replay(
    config: &PipelineConfig,
    records: &[ReplayRecord],
    emit: bool,
) -> Result<ReplayResult> {
    let start_ms = records.first().map_or(0, |record| record.at_ms);
    let mut director = Director::new(
        config.director_policy.clone(),
        config.vlm_advisor.weight,
        config.sync.master_input,
        start_ms,
    );
    let mut result = ReplayResult::default();

    for record in records {
        if let Some(command) = &record.command {
            match command {
                ReplayCommand::Take { input, hold_ms } => {
                    director.take(*input, *hold_ms, record.at_ms)?;
                }
                ReplayCommand::Auto => director.resume_auto(),
                ReplayCommand::Pause => director.pause_auto(),
            }
        }
        let decision = director.evaluate(record.at_ms, &record.cameras, record.vlm.as_ref());
        result.decisions += 1;
        if decision.switched {
            result.switches += 1;
        }
        if emit {
            println!("{}", serde_json::to_string(&decision)?);
        }
    }
    Ok(result)
}

fn explain_graph(config: &PipelineConfig) -> Value {
    json!({
        "apiVersion": config.api_version,
        "pipeline": config.metadata.name,
        "hotPathWaitsForVlm": false,
        "boundedQueues": true,
        "nodes": [
            {
                "id": "srt-input-a",
                "memory": "CPU",
                "description": "runtime-loaded libsrt 1.5 caller/listener adapter"
            },
            {
                "id": "srt-input-b",
                "memory": "CPU",
                "description": "runtime-loaded libsrt 1.5 caller/listener adapter"
            },
            {
                "id": "mpegts-demux",
                "memory": "CPU",
                "description": "streaming sync, PSI/PES reassembly, PTS unwrap and clean-room mux"
            },
            {
                "id": "nvdec-a+b",
                "memory": "CUDA",
                "description": "SDK 13.0 driver probe and RAII surfaces; frame submission pending"
            },
            {
                "id": "sync-buffer",
                "memory": "CPU/CUDA",
                "description": format!(
                    "fixed {}ms buffer; auto pause above {}ms skew",
                    config.sync.buffer_ms, config.sync.max_skew_ms
                )
            },
            {
                "id": "fast-analyzers",
                "memory": "CPU/CUDA",
                "description": "contracts only; realtime analyzers are outside Phase 2"
            },
            {
                "id": "vlm-advisor",
                "memory": "side-channel",
                "description": format!(
                    "{:?}; {}ms deadline; {:.0}% maximum score weight",
                    config.vlm_advisor.mode,
                    config.vlm_advisor.deadline_ms,
                    config.vlm_advisor.weight * 100.0
                )
            },
            {
                "id": "director",
                "memory": "CPU",
                "description": "deterministic state machine; manual take and health gates"
            },
            {
                "id": "audio-switch",
                "memory": "CPU",
                "description": format!(
                    "{}ms equal-power fade to {:.1} LUFS with 4x true-peak limiting",
                    config.audio_switch.crossfade_ms, config.audio_switch.target_lufs
                )
            },
            {
                "id": "nvenc+mpegts+srt",
                "memory": "CUDA/CPU",
                "description": "program clock and TS/SRT ready; codec frame submission pending"
            }
        ],
        "nativeBackendReady": false,
        "implementedNow": [
            "config validation",
            "timeline and bounded synchronization primitives",
            "director state machine",
            "audio loudness, crossfade and 4x true-peak primitives",
            "OpenAI-compatible VLM advisor",
            "streaming MPEG-TS demux/mux with PSI/PES and PTS rollover",
            "runtime-loaded libsrt 1.5 transport boundary",
            "independent program clock and local Unix socket control",
            "NVIDIA and libxaac availability probes",
            "replay, mock runtime and benchmark harness"
        ],
        "pendingForLiveMedia": [
            "NVDEC/NVENC frame submission and NV12 copy",
            "libxaac decoder and encoder frame command sequence",
            "codec-to-scheduler data-plane integration",
            "SRT reconnect, network damage and interoperability qualification"
        ]
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReplayRecord {
    at_ms: u64,
    cameras: [CameraSnapshot; 2],
    #[serde(default)]
    vlm: Option<VlmAdvice>,
    #[serde(default)]
    command: Option<ReplayCommand>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum ReplayCommand {
    Take {
        input: usize,
        #[serde(rename = "holdMs")]
        hold_ms: u64,
    },
    Auto,
    Pause,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReplayResult {
    decisions: u64,
    switches: u64,
}
