//! Intent compilation and typed execution plans.
//!
//! The graph crate is deliberately separate from the executor. It turns a validated media job
//! into an inspectable plan before transports, codecs, or GPU resources are opened.

use aimedia_core::{PipelineConfig, config::VlmMode};
use serde::Serialize;
use thiserror::Error;

pub const PLAN_API_VERSION: &str = "aimedia.plan/v1alpha1";

#[derive(Debug, Error)]
pub enum CompileError {
    #[error("media job configuration is invalid: {0}")]
    InvalidConfig(String),
    #[error("the current graph compiler supports one or two inputs, got {0}")]
    InputCount(usize),
    #[error(
        "input {input:?} declares RTSP correctly, but the RTSP graph adapter is pending V3-02B"
    )]
    RtspAdapterPending { input: String },
    #[error("input {input:?} uses unsupported transport {uri:?}; only SRT is available now")]
    UnsupportedInputTransport { input: String, uri: String },
    #[error("output uses unsupported transport {0:?}; only SRT is available now")]
    UnsupportedOutputTransport(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum JobMode {
    Single,
    Switching,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum NodeKind {
    TransportInput,
    Demux,
    VideoDecoder,
    AudioDecoder,
    Timeline,
    DecisionPolicy,
    AnalyzerTap,
    VideoEncoder,
    AudioEncoder,
    Mux,
    TransportOutput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum NodeStatus {
    Implemented,
    AdapterReady,
    Pending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum MemoryDomain {
    Host,
    NvidiaDevice,
    HostAndNvidia,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum MediaType {
    MpegTs,
    H264AccessUnit,
    AacAdts,
    Nv12Video,
    F32Pcm,
    AnalysisEvent,
    Decision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ClockDomain {
    Source,
    Program,
    Control,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FullPolicy {
    Backpressure,
    DropOldest,
    KeepLatest,
    FailJob,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueContract {
    pub capacity: usize,
    pub full_policy: FullPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortContract {
    pub media: MediaType,
    pub memory: MemoryDomain,
    pub clock: ClockDomain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionNode {
    pub id: String,
    pub kind: NodeKind,
    pub memory: MemoryDomain,
    pub critical: bool,
    pub status: NodeStatus,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionEdge {
    pub from: String,
    pub to: String,
    pub contract: PortContract,
    pub queue: QueueContract,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourcePlan {
    pub gpu_decode_sessions: usize,
    pub gpu_encode_sessions: usize,
    pub independent_program_clock: bool,
    pub all_queues_bounded: bool,
    pub ai_on_hot_path: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionPlan {
    pub api_version: String,
    pub job: String,
    pub mode: JobMode,
    pub declared_buffer_ms: u64,
    pub nodes: Vec<ExecutionNode>,
    pub edges: Vec<ExecutionEdge>,
    pub resources: ResourcePlan,
}

impl ExecutionPlan {
    pub fn pending_nodes(&self) -> impl Iterator<Item = &ExecutionNode> {
        self.nodes
            .iter()
            .filter(|node| node.status == NodeStatus::Pending)
    }

    #[must_use]
    pub fn edge(&self, from: &str, to: &str) -> Option<&ExecutionEdge> {
        self.edges
            .iter()
            .find(|edge| edge.from == from && edge.to == to)
    }

    #[must_use]
    pub fn queue(&self, from: &str, to: &str) -> Option<QueueContract> {
        self.edge(from, to).map(|edge| edge.queue)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueCapacities {
    pub video_frames: usize,
    pub audio_blocks: usize,
    pub transport_messages: usize,
    pub encoded_messages: usize,
}

impl QueueCapacities {
    #[must_use]
    pub fn from_config(config: &PipelineConfig) -> Self {
        let buffer_ms = config.sync.buffer_ms;
        let fps = u64::from(config.media.video.fps);
        // bufferMs is an upper bound for the whole in-engine path, not a fill target. Eight
        // serial scheduling/storage points share it; backpressure preserves data at small sizes.
        let video_budget = ceil_div(buffer_ms.saturating_mul(fps), 1_000);
        let audio_budget = ceil_div(
            buffer_ms.saturating_mul(u64::from(config.media.audio.sample_rate)),
            1_024 * 1_000,
        );
        let video_frames = ceil_div(video_budget, 8).clamp(1, 32) as usize;
        let audio_blocks = ceil_div(audio_budget, 8).clamp(1, 64) as usize;
        Self {
            video_frames,
            audio_blocks,
            transport_messages: video_frames,
            encoded_messages: video_frames,
        }
    }
}

const fn ceil_div(value: u64, divisor: u64) -> u64 {
    value.saturating_add(divisor - 1) / divisor
}

pub fn compile(config: &PipelineConfig) -> Result<ExecutionPlan, CompileError> {
    config
        .validate()
        .map_err(|error| CompileError::InvalidConfig(error.to_string()))?;
    if !(1..=2).contains(&config.inputs.len()) {
        return Err(CompileError::InputCount(config.inputs.len()));
    }
    for input in &config.inputs {
        if is_rtsp(&input.uri) {
            return Err(CompileError::RtspAdapterPending {
                input: input.name.clone(),
            });
        }
        if !is_srt(&input.uri) {
            return Err(CompileError::UnsupportedInputTransport {
                input: input.name.clone(),
                uri: input.uri.clone(),
            });
        }
    }
    if !is_srt(&config.output.uri) {
        return Err(CompileError::UnsupportedOutputTransport(
            config.output.uri.clone(),
        ));
    }

    let capacities = QueueCapacities::from_config(config);
    let mode = if config.inputs.len() == 1 {
        JobMode::Single
    } else {
        JobMode::Switching
    };
    let mut nodes = Vec::new();
    let mut edges = Vec::new();

    for (index, input) in config.inputs.iter().enumerate() {
        let source = format!("input.{index}");
        let demux = format!("demux.{index}");
        let video_decode = format!("video.decode.{index}");
        let audio_decode = format!("audio.decode.{index}");
        nodes.extend([
            node(
                &source,
                NodeKind::TransportInput,
                MemoryDomain::Host,
                true,
                NodeStatus::AdapterReady,
                format!("SRT input {}", input.name),
            ),
            node(
                &demux,
                NodeKind::Demux,
                MemoryDomain::Host,
                true,
                NodeStatus::Implemented,
                "streaming MPEG-TS demux".to_owned(),
            ),
            node(
                &video_decode,
                NodeKind::VideoDecoder,
                MemoryDomain::NvidiaDevice,
                true,
                NodeStatus::AdapterReady,
                "NVDEC H.264 to leased NV12 surface".to_owned(),
            ),
            node(
                &audio_decode,
                NodeKind::AudioDecoder,
                MemoryDomain::Host,
                true,
                NodeStatus::AdapterReady,
                "AAC-LC to interleaved f32 PCM".to_owned(),
            ),
        ]);
        edges.extend([
            edge(
                &source,
                &demux,
                MediaType::MpegTs,
                MemoryDomain::Host,
                ClockDomain::Source,
                capacities.transport_messages,
                FullPolicy::Backpressure,
            ),
            edge(
                &demux,
                &video_decode,
                MediaType::H264AccessUnit,
                MemoryDomain::Host,
                ClockDomain::Source,
                capacities.video_frames,
                FullPolicy::Backpressure,
            ),
            edge(
                &demux,
                &audio_decode,
                MediaType::AacAdts,
                MemoryDomain::Host,
                ClockDomain::Source,
                capacities.audio_blocks,
                FullPolicy::Backpressure,
            ),
        ]);
    }

    nodes.extend([
        node(
            "video.timeline",
            NodeKind::Timeline,
            MemoryDomain::NvidiaDevice,
            true,
            NodeStatus::Implemented,
            "select frames and regenerate the monotonic program timeline".to_owned(),
        ),
        node(
            "audio.timeline",
            NodeKind::Timeline,
            MemoryDomain::Host,
            true,
            NodeStatus::Implemented,
            "select PCM blocks and advance by emitted sample count".to_owned(),
        ),
        node(
            "video.encode",
            NodeKind::VideoEncoder,
            MemoryDomain::NvidiaDevice,
            true,
            NodeStatus::AdapterReady,
            "NVENC H.264 with IDR control".to_owned(),
        ),
        node(
            "audio.encode",
            NodeKind::AudioEncoder,
            MemoryDomain::Host,
            true,
            NodeStatus::AdapterReady,
            "interleaved f32 PCM to AAC-LC".to_owned(),
        ),
        node(
            "mux.program",
            NodeKind::Mux,
            MemoryDomain::Host,
            true,
            NodeStatus::Implemented,
            "MPEG-TS mux with program PTS, DTS and PCR".to_owned(),
        ),
        node(
            "output.program",
            NodeKind::TransportOutput,
            MemoryDomain::Host,
            true,
            NodeStatus::AdapterReady,
            "bounded SRT output".to_owned(),
        ),
    ]);

    for index in 0..config.inputs.len() {
        edges.push(edge(
            &format!("video.decode.{index}"),
            "video.timeline",
            MediaType::Nv12Video,
            MemoryDomain::NvidiaDevice,
            ClockDomain::Source,
            1,
            FullPolicy::Backpressure,
        ));
        edges.push(edge(
            &format!("audio.decode.{index}"),
            "audio.timeline",
            MediaType::F32Pcm,
            MemoryDomain::Host,
            ClockDomain::Source,
            capacities.audio_blocks,
            FullPolicy::Backpressure,
        ));
    }

    if mode == JobMode::Switching {
        nodes.push(node(
            "policy.director",
            NodeKind::DecisionPolicy,
            MemoryDomain::Host,
            false,
            NodeStatus::Implemented,
            "optional deterministic switching policy".to_owned(),
        ));
        edges.extend([
            edge(
                "policy.director",
                "video.timeline",
                MediaType::Decision,
                MemoryDomain::Host,
                ClockDomain::Control,
                1,
                FullPolicy::KeepLatest,
            ),
            edge(
                "policy.director",
                "audio.timeline",
                MediaType::Decision,
                MemoryDomain::Host,
                ClockDomain::Control,
                1,
                FullPolicy::KeepLatest,
            ),
        ]);
    }

    if analyzers_requested(config) {
        nodes.push(node(
            "analysis.tap",
            NodeKind::AnalyzerTap,
            MemoryDomain::HostAndNvidia,
            false,
            NodeStatus::Pending,
            "non-blocking sampled frame, PCM and telemetry tap".to_owned(),
        ));
        for index in 0..config.inputs.len() {
            edges.extend([
                edge(
                    &format!("video.decode.{index}"),
                    "analysis.tap",
                    MediaType::Nv12Video,
                    MemoryDomain::NvidiaDevice,
                    ClockDomain::Source,
                    2,
                    FullPolicy::KeepLatest,
                ),
                edge(
                    &format!("audio.decode.{index}"),
                    "analysis.tap",
                    MediaType::F32Pcm,
                    MemoryDomain::Host,
                    ClockDomain::Source,
                    4,
                    FullPolicy::DropOldest,
                ),
            ]);
        }
        if mode == JobMode::Switching {
            edges.push(edge(
                "analysis.tap",
                "policy.director",
                MediaType::AnalysisEvent,
                MemoryDomain::Host,
                ClockDomain::Control,
                8,
                FullPolicy::DropOldest,
            ));
        }
    }

    edges.extend([
        edge(
            "video.timeline",
            "video.encode",
            MediaType::Nv12Video,
            MemoryDomain::NvidiaDevice,
            ClockDomain::Program,
            1,
            FullPolicy::KeepLatest,
        ),
        edge(
            "audio.timeline",
            "audio.encode",
            MediaType::F32Pcm,
            MemoryDomain::Host,
            ClockDomain::Program,
            capacities.audio_blocks,
            FullPolicy::Backpressure,
        ),
        edge(
            "video.encode",
            "mux.program",
            MediaType::H264AccessUnit,
            MemoryDomain::Host,
            ClockDomain::Program,
            capacities.encoded_messages,
            FullPolicy::Backpressure,
        ),
        edge(
            "audio.encode",
            "mux.program",
            MediaType::AacAdts,
            MemoryDomain::Host,
            ClockDomain::Program,
            capacities.encoded_messages,
            FullPolicy::Backpressure,
        ),
        edge(
            "mux.program",
            "output.program",
            MediaType::MpegTs,
            MemoryDomain::Host,
            ClockDomain::Program,
            capacities.transport_messages,
            FullPolicy::DropOldest,
        ),
    ]);

    Ok(ExecutionPlan {
        api_version: PLAN_API_VERSION.to_owned(),
        job: config.metadata.name.clone(),
        mode,
        declared_buffer_ms: config.sync.buffer_ms,
        nodes,
        edges,
        resources: ResourcePlan {
            gpu_decode_sessions: config.inputs.len(),
            gpu_encode_sessions: 1,
            independent_program_clock: true,
            all_queues_bounded: true,
            ai_on_hot_path: false,
        },
    })
}

fn analyzers_requested(config: &PipelineConfig) -> bool {
    config.fast_analyzers.vad
        || config.fast_analyzers.person
        || config.fast_analyzers.mouth_motion
        || config.fast_analyzers.quality
        || !matches!(config.vlm_advisor.mode, VlmMode::Disabled)
}

fn is_srt(uri: &str) -> bool {
    uri.get(..6)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("srt://"))
}

fn is_rtsp(uri: &str) -> bool {
    uri.get(..7)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("rtsp://"))
}

fn node(
    id: &str,
    kind: NodeKind,
    memory: MemoryDomain,
    critical: bool,
    status: NodeStatus,
    description: String,
) -> ExecutionNode {
    ExecutionNode {
        id: id.to_owned(),
        kind,
        memory,
        critical,
        status,
        description,
    }
}

fn edge(
    from: &str,
    to: &str,
    media: MediaType,
    memory: MemoryDomain,
    clock: ClockDomain,
    capacity: usize,
    full_policy: FullPolicy,
) -> ExecutionEdge {
    ExecutionEdge {
        from: from.to_owned(),
        to: to.to_owned(),
        contract: PortContract {
            media,
            memory,
            clock,
        },
        queue: QueueContract {
            capacity,
            full_policy,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_single_input_into_typed_bounded_plan() {
        let config = PipelineConfig::from_yaml(include_str!("../../../examples/single-srt.yaml"))
            .expect("single input config should be valid");
        let plan = compile(&config).expect("single input plan should compile");

        assert_eq!(plan.mode, JobMode::Single);
        assert_eq!(plan.resources.gpu_decode_sessions, 1);
        assert_eq!(plan.resources.gpu_encode_sessions, 1);
        assert!(plan.resources.all_queues_bounded);
        assert!(!plan.resources.ai_on_hot_path);
        assert!(plan.edges.iter().all(|edge| edge.queue.capacity > 0));
        assert!(plan.nodes.iter().any(|node| node.id == "video.timeline"));
        assert_eq!(
            plan.queue("input.0", "demux.0"),
            Some(QueueContract {
                capacity: 4,
                full_policy: FullPolicy::Backpressure,
            })
        );
        assert_eq!(
            plan.queue("video.decode.0", "video.timeline"),
            Some(QueueContract {
                capacity: 1,
                full_policy: FullPolicy::Backpressure,
            })
        );
        assert_eq!(
            plan.queue("video.timeline", "video.encode"),
            Some(QueueContract {
                capacity: 1,
                full_policy: FullPolicy::KeepLatest,
            })
        );
    }

    #[test]
    fn switching_is_an_optional_policy_node() {
        let config = PipelineConfig::from_yaml(include_str!("../../../examples/director.yaml"))
            .expect("two input config should be valid");
        let plan = compile(&config).expect("two input plan should compile");

        assert_eq!(plan.mode, JobMode::Switching);
        let policy = plan
            .nodes
            .iter()
            .find(|node| node.id == "policy.director")
            .expect("switching plan should include the optional policy");
        assert!(!policy.critical);
        assert!(plan.edges.iter().any(|edge| {
            edge.from == "analysis.tap"
                && edge.to == "policy.director"
                && edge.queue.full_policy == FullPolicy::DropOldest
        }));
    }

    #[test]
    fn reports_rtsp_schema_as_valid_but_adapter_pending() {
        let config = PipelineConfig::from_yaml(include_str!("../../../examples/rtsp.yaml"))
            .expect("RTSP contract should parse before the adapter exists");
        let error = compile(&config).expect_err("RTSP graph must not masquerade as SRT");
        assert!(matches!(error, CompileError::RtspAdapterPending { .. }));
    }
}
