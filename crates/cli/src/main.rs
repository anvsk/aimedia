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
    convert_legacy_yaml,
    vlm::VlmAdvice,
};
use aimedia_graph::{ExecutionPlan, compile as compile_plan};
use aimedia_mpegts::{DemuxEvent, MuxStream, ProgramMap, StreamDemuxer, StreamPacket, probe_path};
use aimedia_nvidia::NvidiaLibraries;
use aimedia_runtime::{run_mock_pipeline, send_control_request};
use aimedia_srt::{Endpoint, SrtTransport, probe_version as probe_srt_version};
use anyhow::{Context, Result, bail};
use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::info;
use tracing_subscriber::EnvFilter;

mod native;

#[derive(Debug, Parser)]
#[command(name = "aimedia", version, about = "Service-native live media runtime")]
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
    /// Validate and run a media job.
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
    /// Inspect or migrate versioned media job configuration.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigAction {
    /// Convert a v1alpha1 DirectorPipeline into a v1alpha2 MediaJob.
    Convert {
        #[arg(short = 'f', long)]
        file: PathBuf,
        /// Write the converted YAML to this path instead of stdout.
        #[arg(short, long)]
        output: Option<PathBuf>,
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
    /// Read current input, health, synchronization, and transport state.
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

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct MonotonicSeries {
    samples: u64,
    regressions: u64,
    first: Option<u64>,
    last: Option<u64>,
    max_gap: u64,
}

impl MonotonicSeries {
    fn observe(&mut self, value: u64) {
        self.first.get_or_insert(value);
        if let Some(previous) = self.last {
            if value < previous {
                self.regressions = self.regressions.saturating_add(1);
            } else {
                self.max_gap = self.max_gap.max(value - previous);
            }
        }
        self.last = Some(value);
        self.samples = self.samples.saturating_add(1);
    }
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct LiveProbeTiming {
    video_packets: u64,
    audio_packets: u64,
    first_video_keyframe: bool,
    video_pts_90khz: MonotonicSeries,
    video_dts_90khz: MonotonicSeries,
    audio_pts_90khz: MonotonicSeries,
    audio_dts_90khz: MonotonicSeries,
    pcr_27mhz: MonotonicSeries,
    #[serde(skip)]
    previous_pcr_raw: Option<u64>,
    #[serde(skip)]
    pcr_wrap_offset: u64,
}

impl LiveProbeTiming {
    fn observe_packet(&mut self, packet: &StreamPacket) {
        match packet.stream {
            MuxStream::Video => {
                if self.video_packets == 0 {
                    self.first_video_keyframe = packet.keyframe;
                }
                self.video_packets = self.video_packets.saturating_add(1);
                self.video_pts_90khz.observe(packet.pts_90khz);
                self.video_dts_90khz
                    .observe(packet.dts_90khz.unwrap_or(packet.pts_90khz));
            }
            MuxStream::Audio => {
                self.audio_packets = self.audio_packets.saturating_add(1);
                self.audio_pts_90khz.observe(packet.pts_90khz);
                self.audio_dts_90khz
                    .observe(packet.dts_90khz.unwrap_or(packet.pts_90khz));
            }
        }
    }

    fn observe_pcr(&mut self, raw: u64) {
        const PCR_MODULUS: u64 = (1_u64 << 33) * 300;
        const PCR_HALF_RANGE: u64 = PCR_MODULUS / 2;
        if self
            .previous_pcr_raw
            .is_some_and(|previous| raw.saturating_add(PCR_HALF_RANGE) < previous)
        {
            self.pcr_wrap_offset = self.pcr_wrap_offset.saturating_add(PCR_MODULUS);
        }
        self.previous_pcr_raw = Some(raw);
        self.pcr_27mhz
            .observe(self.pcr_wrap_offset.saturating_add(raw));
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
        Command::Config { action } => command_config(action),
    }
}

fn command_config(action: ConfigAction) -> Result<()> {
    match action {
        ConfigAction::Convert { file, output } => {
            let contents = fs::read_to_string(&file)
                .with_context(|| format!("failed to read {}", file.display()))?;
            let converted = convert_legacy_yaml(&contents)
                .with_context(|| format!("failed to convert {}", file.display()))?;
            if let Some(output) = output {
                if same_path(&file, &output)? {
                    bail!(
                        "input and output paths must differ; write to a new file, verify it, then replace the legacy file explicitly"
                    );
                }
                fs::write(&output, converted)
                    .with_context(|| format!("failed to write {}", output.display()))?;
                println!("converted MediaJob written to {}", output.display());
            } else {
                print!("{converted}");
            }
            Ok(())
        }
    }
}

fn same_path(left: &Path, right: &Path) -> Result<bool> {
    let left = fs::canonicalize(left)
        .with_context(|| format!("failed to resolve input path {}", left.display()))?;
    let right = if right.exists() {
        fs::canonicalize(right)
            .with_context(|| format!("failed to resolve output path {}", right.display()))?
    } else {
        let parent = right
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let parent = fs::canonicalize(parent)
            .with_context(|| format!("failed to resolve output directory {}", parent.display()))?;
        let file_name = right.file_name().context("output path must name a file")?;
        parent.join(file_name)
    };
    Ok(left == right)
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
    if duration_ms == 0 || duration_ms > 86_400_000 {
        bail!("--duration-ms must be between 1 and 86400000");
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
    let mut timing = LiveProbeTiming::default();

    while started.elapsed() < deadline {
        let chunk = transport.receive().await?;
        bytes = bytes.saturating_add(chunk.data.len() as u64);
        if chunk.discontinuity {
            demuxer = StreamDemuxer::new();
            discontinuities = discontinuities.saturating_add(1);
        }
        for event in demuxer.push(&chunk.data)? {
            match event {
                DemuxEvent::ProgramMap(map) => program_map = Some(map),
                DemuxEvent::Packet(packet) => {
                    media_packets = media_packets.saturating_add(1);
                    timing.observe_packet(&packet);
                }
                DemuxEvent::ClockReference { pcr_27mhz, .. } => {
                    timing.observe_pcr(pcr_27mhz);
                }
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
        if let DemuxEvent::Packet(packet) = event {
            media_packets = media_packets.saturating_add(1);
            timing.observe_packet(&packet);
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
        "timing": timing,
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
    let graph = compile_graph(&config)?;
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
    if is_rtmp_uri(&config.output.uri) {
        bail!(
            "rtmpOutputPendingV3_03E: RTMP/RTMPS publishing is not wired to the native output yet; use an SRT output, or `--dry-run` to inspect this future output plan"
        );
    }
    if config.inputs.len() == 2 {
        bail!(
            "dualDataPlanePending: two-input native execution is scheduled for v0.3; use a \
             single-input configuration for the v0.2 data plane, or `--mock` for control testing"
        );
    }
    native::run(config).await
}

fn is_rtmp_uri(uri: &str) -> bool {
    uri.get(..7)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("rtmp://"))
        || uri
            .get(..8)
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("rtmps://"))
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
    let graph = compile_graph(&config)?;
    if output_json {
        println!("{}", serde_json::to_string_pretty(&graph)?);
    } else {
        println!("job: {}", graph.job);
        println!("mode: {:?}", graph.mode);
        println!(
            "resources: {} decode session(s), {} encode session; bounded queues={}, AI on hot path={}",
            graph.resources.gpu_decode_sessions,
            graph.resources.gpu_encode_sessions,
            graph.resources.all_queues_bounded,
            graph.resources.ai_on_hot_path
        );
        println!("nodes:");
        for node in &graph.nodes {
            println!(
                "  {:<20} {:<16?} {:<14?} {}",
                node.id, node.kind, node.status, node.description
            );
        }
        println!("edges:");
        for edge in &graph.edges {
            println!(
                "  {:<20} -> {:<20} {:?}/{:?}/{:?}, queue={} {:?}",
                edge.from,
                edge.to,
                edge.contract.media,
                edge.contract.memory,
                edge.contract.clock,
                edge.queue.capacity,
                edge.queue.full_policy
            );
        }
        let pending = graph
            .pending_nodes()
            .map(|node| node.id.as_str())
            .collect::<Vec<_>>();
        if !pending.is_empty() {
            println!("pending backends: {}", pending.join(", "));
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

fn compile_graph(config: &PipelineConfig) -> Result<ExecutionPlan> {
    compile_plan(config).context("media job could not be compiled into an execution plan")
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
