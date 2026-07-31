use std::{
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

use aimedia_core::{CameraSnapshot, Director, PipelineConfig, vlm::VlmAdvice};
use aimedia_mpegts::probe_path;
use anyhow::{Context, Result, bail};
use clap::{ArgAction, Parser, Subcommand};
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
    /// Probe an MPEG-TS capture. Live SRT probing arrives with the libsrt backend.
    Probe {
        source: String,
        #[arg(long)]
        json: bool,
    },
    /// Validate and run a director pipeline.
    Run {
        #[arg(short = 'f', long)]
        file: PathBuf,
        /// Validate the complete graph without opening transports or hardware.
        #[arg(long)]
        dry_run: bool,
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
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    match cli.command {
        Command::Probe { source, json } => command_probe(&source, json),
        Command::Run { file, dry_run } => command_run(&file, dry_run),
        Command::Explain { file, json } => command_explain(&file, json),
        Command::Replay { capture, file } => command_replay(&file, &capture),
        Command::Bench {
            file,
            capture,
            iterations,
        } => command_bench(&file, &capture, iterations),
    }
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

fn command_probe(source: &str, output_json: bool) -> Result<()> {
    if source.starts_with("srt://") {
        bail!(
            "live SRT probing requires the planned libsrt backend; use a local MPEG-TS capture in \
             this foundation build"
        );
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

fn command_run(path: &Path, dry_run: bool) -> Result<()> {
    let config = load_config(path)?;
    let graph = explain_graph(&config);
    if dry_run {
        println!("{}", serde_json::to_string_pretty(&graph)?);
        info!(pipeline = %config.metadata.name, "configuration and graph are valid");
        return Ok(());
    }
    bail!(
        "native live execution is intentionally unavailable in this foundation build: \
         libsrt, NVDEC/NVENC, ONNX Runtime, and libxaac adapters have contracts but are not linked; \
         use `aimedia replay` to exercise the director or `aimedia run --dry-run` to validate a graph"
    )
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
                "description": "libsrt transport adapter (planned native backend)"
            },
            {
                "id": "srt-input-b",
                "memory": "CPU",
                "description": "libsrt transport adapter (planned native backend)"
            },
            {
                "id": "mpegts-demux",
                "memory": "CPU",
                "description": "clean-room MPEG-TS parser; probe path implemented"
            },
            {
                "id": "nvdec-a+b",
                "memory": "CUDA",
                "description": "two continuous H.264 decode sessions (adapter contract)"
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
                "description": "VAD, person, mouth motion, quality and transport health"
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
                    "{}ms equal-power fade to {:.1} LUFS",
                    config.audio_switch.crossfade_ms, config.audio_switch.target_lufs
                )
            },
            {
                "id": "nvenc+mpegts+srt",
                "memory": "CUDA/CPU",
                "description": "single monotonic program encoder and output transport"
            }
        ],
        "nativeBackendReady": false,
        "implementedNow": [
            "config validation",
            "timeline and bounded synchronization primitives",
            "director state machine",
            "audio loudness and crossfade primitives",
            "OpenAI-compatible VLM advisor",
            "MPEG-TS packet/PAT/PMT/PCR probe",
            "replay and benchmark harness"
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
