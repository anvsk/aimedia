//! Program scheduling, bounded-capacity calculations, and the local control plane.

pub mod single;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use aimedia_core::{
    CameraSnapshot, ControlCommand, ControlErrorCode, ControlRequest, ControlResponse, Director,
    FastSignals, GpuSurfaceRuntimeStats, InputCodecRuntimeStats, InputRuntimeState,
    OutputRuntimeState, PipelineConfig, PipelineMode, PipelineRuntimeState, QueueRuntimeState,
    SrtRuntimeStats, SwitchReason, backend::CodecId,
};
pub use aimedia_graph::QueueCapacities;
use aimedia_graph::{
    CompileError, ExecutionPlan, FullPolicy, JobMode, PLAN_API_VERSION, compile as compile_plan,
};
use thiserror::Error;
use tokio::sync::Mutex;

#[cfg(unix)]
use aimedia_core::config::parse_socket_mode;

#[cfg(unix)]
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("control socket is supported only on Unix")]
    UnsupportedPlatform,
    #[error("control socket I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("control protocol failed: {0}")]
    Protocol(#[from] serde_json::Error),
    #[error("control socket mode {0:?} is invalid")]
    InvalidSocketMode(String),
    #[error("refusing to replace non-socket path {0}")]
    UnsafeSocketPath(PathBuf),
    #[error("control server task failed: {0}")]
    Join(String),
    #[error("execution plan failed preflight: {0}")]
    Plan(#[from] RuntimePlanError),
}

#[derive(Debug, Error)]
pub enum RuntimePlanError {
    #[error("media job could not be compiled: {0}")]
    Compile(#[from] CompileError),
    #[error("execution plan {field} mismatch: expected {expected:?}, got {actual:?}")]
    Identity {
        field: &'static str,
        expected: String,
        actual: String,
    },
    #[error("execution plan is missing required edge {from:?} -> {to:?}")]
    MissingEdge { from: String, to: String },
    #[error(
        "execution edge {from:?} -> {to:?} uses {actual:?}; the current executor requires {expected:?}"
    )]
    QueuePolicy {
        from: String,
        to: String,
        expected: FullPolicy,
        actual: FullPolicy,
    },
    #[error("execution edge {from:?} -> {to:?} has zero queue capacity")]
    ZeroCapacity { from: String, to: String },
    #[error("execution plan invariant failed: {0}")]
    Invariant(String),
}

/// Independent output clock. Video uses a rational accumulator; audio uses exact sample counts.
#[derive(Debug, Clone)]
pub struct ProgramClock {
    fps_numerator: u64,
    fps_denominator: u64,
    video_index: u64,
    audio_samples: u64,
}

impl ProgramClock {
    #[must_use]
    pub const fn new(fps_numerator: u64, fps_denominator: u64) -> Self {
        assert!(fps_numerator > 0, "fps numerator must be non-zero");
        assert!(fps_denominator > 0, "fps denominator must be non-zero");
        Self {
            fps_numerator,
            fps_denominator,
            video_index: 0,
            audio_samples: 0,
        }
    }

    #[must_use]
    pub fn next_video_pts_90khz(&mut self) -> u64 {
        let pts = u128::from(self.video_index)
            .saturating_mul(90_000)
            .saturating_mul(u128::from(self.fps_denominator))
            / u128::from(self.fps_numerator);
        self.video_index = self.video_index.saturating_add(1);
        pts.min(u128::from(u64::MAX)) as u64
    }

    #[must_use]
    pub fn next_audio_pts_90khz(&mut self, samples: u32, sample_rate: u32) -> u64 {
        assert!(sample_rate > 0, "sample rate must be non-zero");
        let pts = u128::from(self.audio_samples).saturating_mul(90_000) / u128::from(sample_rate);
        self.audio_samples = self.audio_samples.saturating_add(u64::from(samples));
        pts.min(u128::from(u64::MAX)) as u64
    }
}

/// Limits timestamp mapping changes to one millisecond per elapsed second.
#[derive(Debug, Clone)]
pub struct DriftCorrector {
    correction_micros: i64,
    last_update_ms: u64,
}

impl DriftCorrector {
    #[must_use]
    pub const fn new(now_ms: u64) -> Self {
        Self {
            correction_micros: 0,
            last_update_ms: now_ms,
        }
    }

    pub fn update(&mut self, measured_skew_ms: i64, now_ms: u64) -> i64 {
        let elapsed_ms = now_ms.saturating_sub(self.last_update_ms);
        self.last_update_ms = now_ms;
        let maximum_change_micros = i64::try_from(elapsed_ms).unwrap_or(i64::MAX);
        let desired_micros = measured_skew_ms.saturating_mul(-1_000);
        let change = desired_micros
            .saturating_sub(self.correction_micros)
            .clamp(-maximum_change_micros, maximum_change_micros);
        self.correction_micros = self.correction_micros.saturating_add(change);
        self.correction_micros
    }

    #[must_use]
    pub const fn correction_micros(&self) -> i64 {
        self.correction_micros
    }
}

#[derive(Debug)]
struct Controller {
    pipeline: String,
    started: Instant,
    running: bool,
    input_count: usize,
    director: Director,
    cameras: [CameraSnapshot; 2],
    input_states: [InputRuntimeState; 2],
    output_state: OutputRuntimeState,
    queues: Vec<QueueRuntimeState>,
    last_reason: SwitchReason,
}

impl Controller {
    fn new(
        config: &PipelineConfig,
        plan: &ExecutionPlan,
        healthy: bool,
    ) -> Result<Self, RuntimePlanError> {
        validate_runtime_plan(config, plan)?;
        let input_count = config.inputs.len();
        let cameras = std::array::from_fn(|index| CameraSnapshot {
            name: config.inputs.get(index).map_or_else(
                || format!("unconfigured-{index}"),
                |input| input.name.clone(),
            ),
            fast: FastSignals {
                vad: 0.0,
                mouth_motion: 0.0,
                composition: 0.5,
                quality: if index < input_count { 1.0 } else { 0.0 },
                transport_health: if healthy && index < input_count {
                    1.0
                } else {
                    0.0
                },
            },
            healthy: healthy && index < input_count,
            synchronized: healthy && index < input_count,
            frozen: false,
            skew_ms: 0,
        });
        let input_states = std::array::from_fn(|index| InputRuntimeState {
            name: cameras[index].name.clone(),
            healthy: healthy && index < input_count,
            synchronized: healthy && index < input_count,
            frozen: false,
            skew_ms: 0,
            video_timeline_depth: 0,
            audio_timeline_depth: 0,
            srt: SrtRuntimeStats {
                connected: healthy && index < input_count,
                ..SrtRuntimeStats::default()
            },
            codec: InputCodecRuntimeStats::default(),
            gpu: GpuSurfaceRuntimeStats::default(),
        });
        let queues = controller_queue_states(config, plan)?;
        Ok(Self {
            pipeline: config.metadata.name.clone(),
            started: Instant::now(),
            running: true,
            input_count,
            director: Director::new(
                config.director_policy.clone(),
                config.vlm_advisor.weight,
                config.sync.master_input,
                0,
            ),
            cameras,
            input_states,
            output_state: OutputRuntimeState::default(),
            queues,
            last_reason: SwitchReason::Initial,
        })
    }

    fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
    }

    fn tick(&mut self) {
        let now_ms = self.elapsed_ms();
        let decision = self.director.evaluate(now_ms, &self.cameras, None);
        self.last_reason = decision.reason;
    }

    fn state(&self) -> PipelineRuntimeState {
        let active = self.director.active_input();
        PipelineRuntimeState {
            pipeline: self.pipeline.clone(),
            running: self.running,
            active_input: active,
            active_name: self.cameras[active].name.clone(),
            mode: if self.input_count == 1 {
                PipelineMode::Single
            } else if self.director.auto_enabled() {
                PipelineMode::Automatic
            } else {
                PipelineMode::Manual
            },
            hold_until_ms: if self.input_count == 1 {
                None
            } else {
                self.director.hold_until_ms()
            },
            inputs: self.input_states[..self.input_count].to_vec(),
            output: self.output_state.clone(),
            queues: self.queues.clone(),
            last_reason: format!("{:?}", self.last_reason),
        }
    }

    fn process(&mut self, request: ControlRequest) -> ControlResponse {
        if request.api_version != aimedia_core::control::CONTROL_API_VERSION {
            return ControlResponse::rejected(
                request.request_id,
                ControlErrorCode::UnsupportedVersion,
                format!("unsupported apiVersion {:?}", request.api_version),
                Some(self.state()),
            );
        }
        if request.request_id.trim().is_empty() || request.request_id.len() > 128 {
            return ControlResponse::rejected(
                request.request_id,
                ControlErrorCode::InvalidRequest,
                "requestId must contain 1 to 128 characters",
                Some(self.state()),
            );
        }

        match request.command {
            ControlCommand::Take { input, hold_ms } => {
                if self.input_count == 1 {
                    return ControlResponse::rejected(
                        request.request_id,
                        ControlErrorCode::NotApplicable,
                        "take is not applicable to a single-input pipeline",
                        Some(self.state()),
                    );
                }
                if hold_ms != 0 && !(100..=3_600_000).contains(&hold_ms) {
                    return ControlResponse::rejected(
                        request.request_id,
                        ControlErrorCode::InvalidHold,
                        "holdMs must be 0 or between 100 and 3600000",
                        Some(self.state()),
                    );
                }
                let Some(index) = self.cameras.iter().position(|camera| camera.name == input)
                else {
                    return ControlResponse::rejected(
                        request.request_id,
                        ControlErrorCode::UnknownInput,
                        format!("unknown input {input:?}"),
                        Some(self.state()),
                    );
                };
                if !self.cameras[index].eligible() {
                    return ControlResponse::rejected(
                        request.request_id,
                        ControlErrorCode::TargetUnavailable,
                        format!("input {input:?} is unhealthy, frozen, or unsynchronized"),
                        Some(self.state()),
                    );
                }
                let now_ms = self.elapsed_ms();
                if let Err(error) = self.director.take(index, hold_ms, now_ms) {
                    return ControlResponse::rejected(
                        request.request_id,
                        ControlErrorCode::InvalidRequest,
                        error.to_string(),
                        Some(self.state()),
                    );
                }
                self.tick();
                ControlResponse::accepted(request.request_id, self.state())
            }
            ControlCommand::Auto => {
                if self.input_count == 1 {
                    return ControlResponse::rejected(
                        request.request_id,
                        ControlErrorCode::NotApplicable,
                        "auto is not applicable to a single-input pipeline",
                        Some(self.state()),
                    );
                }
                self.director.resume_auto();
                self.tick();
                ControlResponse::accepted(request.request_id, self.state())
            }
            ControlCommand::State => {
                self.tick();
                ControlResponse::accepted(request.request_id, self.state())
            }
        }
    }

    fn set_input_state(&mut self, index: usize, update: InputRuntimeState) {
        if index >= self.input_count {
            return;
        }
        self.cameras[index].healthy = update.healthy;
        self.cameras[index].synchronized = update.synchronized;
        self.cameras[index].frozen = update.frozen;
        self.cameras[index].skew_ms = update.skew_ms;
        self.cameras[index].fast.transport_health = if update.healthy { 1.0 } else { 0.0 };
        self.input_states[index] = update;
        self.tick();
    }

    fn observe_queue(&mut self, name: &str, depth: usize) {
        for queue in self.queues.iter_mut().filter(|queue| queue.name == name) {
            queue.depth = depth.min(queue.capacity);
            queue.high_watermark = queue.high_watermark.max(queue.depth);
        }
        for (index, state) in self.input_states[..self.input_count].iter_mut().enumerate() {
            let input = &self.cameras[index].name;
            if name.strip_suffix(".videoTimeline") == Some(input.as_str()) {
                state.video_timeline_depth = depth;
            } else if name.strip_suffix(".audioTimeline") == Some(input.as_str()) {
                state.audio_timeline_depth = depth;
            }
        }
    }

    fn set_input_srt(&mut self, index: usize, stats: SrtRuntimeStats) {
        if index >= self.input_count {
            return;
        }
        if !stats.connected {
            self.input_states[index].healthy = false;
            self.input_states[index].synchronized = false;
            self.cameras[index].healthy = false;
            self.cameras[index].synchronized = false;
            self.cameras[index].fast.transport_health = 0.0;
        }
        self.input_states[index].srt = stats;
    }

    fn set_output_srt(&mut self, stats: SrtRuntimeStats) {
        self.output_state.srt = stats;
    }

    fn set_gpu_surfaces(&mut self, index: usize, stats: GpuSurfaceRuntimeStats) {
        if index < self.input_count {
            self.input_states[index].gpu = stats;
        }
    }

    fn mark_input_discontinuity(&mut self, index: usize) {
        if index >= self.input_count {
            return;
        }
        self.input_states[index].healthy = false;
        self.input_states[index].synchronized = false;
        self.cameras[index].healthy = false;
        self.cameras[index].synchronized = false;
        self.cameras[index].fast.transport_health = 0.0;
    }

    fn record_decoded(&mut self, codec: CodecId, count: usize) {
        let count = count as u64;
        match codec {
            CodecId::H264 => {
                self.input_states[0].codec.video_decoded_frames = self.input_states[0]
                    .codec
                    .video_decoded_frames
                    .saturating_add(count);
                if count > 0 {
                    self.input_states[0].healthy = true;
                    self.input_states[0].synchronized = true;
                    self.cameras[0].healthy = true;
                    self.cameras[0].synchronized = true;
                    self.cameras[0].fast.transport_health = 1.0;
                }
            }
            CodecId::AacLc => {
                self.input_states[0].codec.audio_decoded_frames = self.input_states[0]
                    .codec
                    .audio_decoded_frames
                    .saturating_add(count);
            }
            CodecId::PcmF32 | CodecId::Unknown(_) => {}
        }
    }

    fn record_input_drop(&mut self, codec: CodecId) {
        match codec {
            CodecId::H264 => {
                self.input_states[0].codec.video_dropped_frames = self.input_states[0]
                    .codec
                    .video_dropped_frames
                    .saturating_add(1);
            }
            CodecId::AacLc => {
                self.input_states[0].codec.audio_dropped_frames = self.input_states[0]
                    .codec
                    .audio_dropped_frames
                    .saturating_add(1);
            }
            CodecId::PcmF32 | CodecId::Unknown(_) => {}
        }
    }

    fn record_encoded(&mut self, codec: CodecId) {
        match codec {
            CodecId::H264 => {
                self.output_state.video_encoded_frames =
                    self.output_state.video_encoded_frames.saturating_add(1);
            }
            CodecId::AacLc => {
                self.output_state.audio_encoded_frames =
                    self.output_state.audio_encoded_frames.saturating_add(1);
            }
            CodecId::PcmF32 | CodecId::Unknown(_) => {}
        }
    }

    fn record_output_drop(&mut self, codec: CodecId) {
        match codec {
            CodecId::H264 => {
                self.output_state.video_dropped_frames =
                    self.output_state.video_dropped_frames.saturating_add(1);
            }
            CodecId::AacLc => {
                self.output_state.audio_dropped_frames =
                    self.output_state.audio_dropped_frames.saturating_add(1);
            }
            CodecId::PcmF32 | CodecId::Unknown(_) => {}
        }
    }

    fn finish(&mut self) {
        self.running = false;
        for queue in &mut self.queues {
            queue.depth = 0;
        }
    }
}

fn queue_state(
    name: impl Into<String>,
    from: impl Into<String>,
    to: impl Into<String>,
    full_policy: FullPolicy,
    capacity: usize,
) -> QueueRuntimeState {
    QueueRuntimeState {
        name: name.into(),
        from: from.into(),
        to: to.into(),
        full_policy: full_policy_name(full_policy).to_owned(),
        depth: 0,
        capacity,
        high_watermark: 0,
    }
}

const fn full_policy_name(policy: FullPolicy) -> &'static str {
    match policy {
        FullPolicy::Backpressure => "backpressure",
        FullPolicy::DropOldest => "dropOldest",
        FullPolicy::KeepLatest => "keepLatest",
        FullPolicy::FailJob => "failJob",
    }
}

pub(crate) fn validate_runtime_plan(
    config: &PipelineConfig,
    plan: &ExecutionPlan,
) -> Result<(), RuntimePlanError> {
    if plan.api_version != PLAN_API_VERSION {
        return Err(RuntimePlanError::Identity {
            field: "apiVersion",
            expected: PLAN_API_VERSION.to_owned(),
            actual: plan.api_version.clone(),
        });
    }
    if plan.job != config.metadata.name {
        return Err(RuntimePlanError::Identity {
            field: "job",
            expected: config.metadata.name.clone(),
            actual: plan.job.clone(),
        });
    }
    let expected_mode = if config.inputs.len() == 1 {
        JobMode::Single
    } else {
        JobMode::Switching
    };
    if plan.mode != expected_mode {
        return Err(RuntimePlanError::Identity {
            field: "mode",
            expected: format!("{expected_mode:?}"),
            actual: format!("{:?}", plan.mode),
        });
    }
    if plan.declared_buffer_ms != config.sync.buffer_ms {
        return Err(RuntimePlanError::Identity {
            field: "declaredBufferMs",
            expected: config.sync.buffer_ms.to_string(),
            actual: plan.declared_buffer_ms.to_string(),
        });
    }
    if plan.resources.gpu_decode_sessions != config.inputs.len() {
        return Err(RuntimePlanError::Invariant(format!(
            "expected {} GPU decode session(s), got {}",
            config.inputs.len(),
            plan.resources.gpu_decode_sessions
        )));
    }
    if plan.resources.gpu_encode_sessions != 1 {
        return Err(RuntimePlanError::Invariant(format!(
            "expected one GPU encode session, got {}",
            plan.resources.gpu_encode_sessions
        )));
    }
    if !plan.resources.independent_program_clock {
        return Err(RuntimePlanError::Invariant(
            "the executor requires an independent program clock".to_owned(),
        ));
    }
    if !plan.resources.all_queues_bounded {
        return Err(RuntimePlanError::Invariant(
            "the executor refuses plans with unbounded queues".to_owned(),
        ));
    }
    if plan.resources.ai_on_hot_path {
        return Err(RuntimePlanError::Invariant(
            "AI cannot be placed on the media hot path".to_owned(),
        ));
    }
    Ok(())
}

pub(crate) fn plan_queue_capacity(
    plan: &ExecutionPlan,
    from: &str,
    to: &str,
    expected_policy: FullPolicy,
) -> Result<usize, RuntimePlanError> {
    let queue = plan
        .queue(from, to)
        .ok_or_else(|| RuntimePlanError::MissingEdge {
            from: from.to_owned(),
            to: to.to_owned(),
        })?;
    if queue.capacity == 0 {
        return Err(RuntimePlanError::ZeroCapacity {
            from: from.to_owned(),
            to: to.to_owned(),
        });
    }
    if queue.full_policy != expected_policy {
        return Err(RuntimePlanError::QueuePolicy {
            from: from.to_owned(),
            to: to.to_owned(),
            expected: expected_policy,
            actual: queue.full_policy,
        });
    }
    Ok(queue.capacity)
}

fn controller_queue_states(
    config: &PipelineConfig,
    plan: &ExecutionPlan,
) -> Result<Vec<QueueRuntimeState>, RuntimePlanError> {
    for index in 0..config.inputs.len() {
        for (from, to) in [
            (format!("input.{index}"), format!("demux.{index}")),
            (format!("demux.{index}"), format!("video.decode.{index}")),
            (format!("demux.{index}"), format!("audio.decode.{index}")),
            (format!("video.decode.{index}"), "video.timeline".to_owned()),
            (format!("audio.decode.{index}"), "audio.timeline".to_owned()),
        ] {
            plan_queue_capacity(plan, &from, &to, FullPolicy::Backpressure)?;
        }
    }
    let video_timeline = plan_queue_capacity(
        plan,
        "video.timeline",
        "video.encode",
        FullPolicy::Backpressure,
    )?;
    let audio_timeline = plan_queue_capacity(
        plan,
        "audio.timeline",
        "audio.encode",
        FullPolicy::Backpressure,
    )?;
    let video_output = plan_queue_capacity(
        plan,
        "video.encode",
        "mux.program",
        FullPolicy::Backpressure,
    )?;
    let audio_output = plan_queue_capacity(
        plan,
        "audio.encode",
        "mux.program",
        FullPolicy::Backpressure,
    )?;
    let transport_output = plan_queue_capacity(
        plan,
        "mux.program",
        "output.program",
        FullPolicy::DropOldest,
    )?;
    if config.inputs.len() == 1 {
        let input_video = plan_queue_capacity(
            plan,
            "video.decode.0",
            "video.timeline",
            FullPolicy::Backpressure,
        )?;
        let input_audio = plan_queue_capacity(
            plan,
            "audio.decode.0",
            "audio.timeline",
            FullPolicy::Backpressure,
        )?;
        if input_video != video_timeline || input_audio != audio_timeline {
            return Err(RuntimePlanError::Invariant(
                "fused timeline queues require equal input/output capacities".to_owned(),
            ));
        }
        if video_output != audio_output || video_output != transport_output {
            return Err(RuntimePlanError::Invariant(
                "the fused program output queue requires equal encoded/transport capacities"
                    .to_owned(),
            ));
        }
    }
    Ok(plan
        .edges
        .iter()
        .map(|edge| {
            queue_state(
                physical_queue_name(config, &edge.from, &edge.to),
                edge.from.clone(),
                edge.to.clone(),
                edge.queue.full_policy,
                edge.queue.capacity,
            )
        })
        .collect())
}

fn physical_queue_name(config: &PipelineConfig, from: &str, to: &str) -> String {
    if config.inputs.len() == 1 {
        let input = &config.inputs[0].name;
        return match (from, to) {
            ("input.0", "demux.0") => format!("{input}.transport"),
            ("demux.0", "video.decode.0") => format!("{input}.videoDecode"),
            ("demux.0", "audio.decode.0") => format!("{input}.audioDecode"),
            ("video.decode.0", "video.timeline") | ("video.timeline", "video.encode") => {
                format!("{input}.videoTimeline")
            }
            ("audio.decode.0", "audio.timeline") | ("audio.timeline", "audio.encode") => {
                format!("{input}.audioTimeline")
            }
            ("video.encode", "mux.program")
            | ("audio.encode", "mux.program")
            | ("mux.program", "output.program") => "program.output".to_owned(),
            _ => format!("{from}->{to}"),
        };
    }
    format!("{from}->{to}")
}

#[derive(Debug, Clone)]
pub struct ControllerHandle {
    inner: Arc<Mutex<Controller>>,
}

impl ControllerHandle {
    pub fn new(config: &PipelineConfig, inputs_healthy: bool) -> Result<Self, RuntimePlanError> {
        let plan = compile_plan(config)?;
        Self::from_plan(config, &plan, inputs_healthy)
    }

    pub fn from_plan(
        config: &PipelineConfig,
        plan: &ExecutionPlan,
        inputs_healthy: bool,
    ) -> Result<Self, RuntimePlanError> {
        Ok(Self {
            inner: Arc::new(Mutex::new(Controller::new(config, plan, inputs_healthy)?)),
        })
    }

    pub async fn process(&self, request: ControlRequest) -> ControlResponse {
        self.inner.lock().await.process(request)
    }

    pub async fn tick(&self) {
        self.inner.lock().await.tick();
    }

    pub async fn state(&self) -> PipelineRuntimeState {
        self.inner.lock().await.state()
    }

    pub async fn set_input_state(&self, index: usize, update: InputRuntimeState) {
        self.inner.lock().await.set_input_state(index, update);
    }

    pub(crate) async fn observe_queue(&self, name: &str, depth: usize) {
        self.inner.lock().await.observe_queue(name, depth);
    }

    pub(crate) async fn set_input_srt(&self, index: usize, stats: SrtRuntimeStats) {
        self.inner.lock().await.set_input_srt(index, stats);
    }

    pub(crate) async fn set_output_srt(&self, stats: SrtRuntimeStats) {
        self.inner.lock().await.set_output_srt(stats);
    }

    pub(crate) async fn set_gpu_surfaces(&self, index: usize, stats: GpuSurfaceRuntimeStats) {
        self.inner.lock().await.set_gpu_surfaces(index, stats);
    }

    pub(crate) async fn mark_input_discontinuity(&self, index: usize) {
        self.inner.lock().await.mark_input_discontinuity(index);
    }

    pub(crate) async fn record_decoded(&self, codec: CodecId, count: usize) {
        self.inner.lock().await.record_decoded(codec, count);
    }

    pub(crate) async fn record_input_drop(&self, codec: CodecId) {
        self.inner.lock().await.record_input_drop(codec);
    }

    pub(crate) async fn record_encoded(&self, codec: CodecId) {
        self.inner.lock().await.record_encoded(codec);
    }

    pub(crate) async fn record_output_drop(&self, codec: CodecId) {
        self.inner.lock().await.record_output_drop(codec);
    }

    pub(crate) async fn finish(&self) {
        self.inner.lock().await.finish();
    }
}

#[derive(Debug)]
pub struct ControlServer {
    #[cfg_attr(not(unix), allow(dead_code))]
    socket_path: PathBuf,
    task: tokio::task::JoinHandle<Result<(), RuntimeError>>,
}

impl ControlServer {
    #[cfg(unix)]
    pub async fn start(
        socket_path: impl AsRef<Path>,
        socket_mode: &str,
        controller: ControllerHandle,
    ) -> Result<Self, RuntimeError> {
        use std::os::unix::fs::{FileTypeExt, PermissionsExt};

        let socket_path = socket_path.as_ref().to_path_buf();
        let mode = parse_socket_mode(socket_mode)
            .ok_or_else(|| RuntimeError::InvalidSocketMode(socket_mode.to_owned()))?;
        if let Some(parent) = socket_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        if let Ok(metadata) = tokio::fs::symlink_metadata(&socket_path).await {
            if !metadata.file_type().is_socket() {
                return Err(RuntimeError::UnsafeSocketPath(socket_path));
            }
            tokio::fs::remove_file(&socket_path).await?;
        }
        let listener = UnixListener::bind(&socket_path)?;
        tokio::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(mode)).await?;
        let task_path = socket_path.clone();
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await?;
                let connection_controller = controller.clone();
                tokio::spawn(async move {
                    if let Err(error) = serve_connection(stream, connection_controller).await {
                        tracing::warn!(%error, "control connection closed with an error");
                    }
                });
            }
            #[allow(unreachable_code)]
            Ok::<(), RuntimeError>(())
        });
        tracing::info!(path = %task_path.display(), mode = %socket_mode, "control socket ready");
        Ok(Self { socket_path, task })
    }

    #[cfg(not(unix))]
    pub async fn start(
        _socket_path: impl AsRef<Path>,
        _socket_mode: &str,
        _controller: ControllerHandle,
    ) -> Result<Self, RuntimeError> {
        Err(RuntimeError::UnsupportedPlatform)
    }

    pub async fn shutdown(self) -> Result<(), RuntimeError> {
        self.task.abort();
        let _ = self.task.await;
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileTypeExt;
            if let Ok(metadata) = tokio::fs::symlink_metadata(&self.socket_path).await {
                if metadata.file_type().is_socket() {
                    tokio::fs::remove_file(&self.socket_path).await?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
async fn serve_connection(
    stream: UnixStream,
    controller: ControllerHandle,
) -> Result<(), RuntimeError> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    loop {
        let line = match read_control_line(&mut reader).await? {
            ControlLine::Eof => break,
            ControlLine::TooLarge => {
                let response = ControlResponse::rejected(
                    "",
                    ControlErrorCode::InvalidRequest,
                    "control request exceeds 65536 bytes",
                    None,
                );
                write_response(&mut writer, &response).await?;
                break;
            }
            ControlLine::Line(line) => line,
        };
        if line.is_empty() {
            let response = ControlResponse::rejected(
                "",
                ControlErrorCode::InvalidRequest,
                "control request must not be empty",
                None,
            );
            write_response(&mut writer, &response).await?;
            continue;
        }
        let response = match serde_json::from_slice::<ControlRequest>(&line) {
            Ok(request) => controller.process(request).await,
            Err(error) => ControlResponse::rejected(
                "",
                ControlErrorCode::InvalidRequest,
                format!("invalid control JSON: {error}"),
                None,
            ),
        };
        write_response(&mut writer, &response).await?;
    }
    Ok(())
}

#[cfg(unix)]
enum ControlLine {
    Eof,
    Line(Vec<u8>),
    TooLarge,
}

#[cfg(unix)]
async fn read_control_line(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
) -> Result<ControlLine, RuntimeError> {
    const MAX_CONTROL_LINE_BYTES: usize = 65_536;
    let mut line = Vec::with_capacity(1_024);
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(ControlLine::Eof)
            } else {
                Ok(ControlLine::Line(line))
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let payload_bytes = newline.unwrap_or(available.len());
        if line.len().saturating_add(payload_bytes) > MAX_CONTROL_LINE_BYTES {
            return Ok(ControlLine::TooLarge);
        }
        line.extend_from_slice(&available[..payload_bytes]);
        let consumed = payload_bytes + usize::from(newline.is_some());
        reader.consume(consumed);
        if newline.is_some() {
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(ControlLine::Line(line));
        }
    }
}

#[cfg(unix)]
async fn write_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    response: &ControlResponse,
) -> Result<(), RuntimeError> {
    let mut encoded = serde_json::to_vec(response)?;
    encoded.push(b'\n');
    writer.write_all(&encoded).await?;
    Ok(())
}

#[cfg(unix)]
pub async fn send_control_request(
    socket_path: impl AsRef<Path>,
    request: &ControlRequest,
) -> Result<ControlResponse, RuntimeError> {
    let mut stream = UnixStream::connect(socket_path).await?;
    let mut encoded = serde_json::to_vec(request)?;
    encoded.push(b'\n');
    stream.write_all(&encoded).await?;
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).await?;
    Ok(serde_json::from_str(&response)?)
}

#[cfg(not(unix))]
pub async fn send_control_request(
    _socket_path: impl AsRef<Path>,
    _request: &ControlRequest,
) -> Result<ControlResponse, RuntimeError> {
    Err(RuntimeError::UnsupportedPlatform)
}

pub async fn run_mock_pipeline(config: PipelineConfig) -> Result<(), RuntimeError> {
    let controller = ControllerHandle::new(&config, true)?;
    let server = ControlServer::start(
        &config.control.socket_path,
        &config.control.socket_mode,
        controller.clone(),
    )
    .await?;
    let period = Duration::from_secs_f64(1.0 / f64::from(config.media.video.fps));
    let mut interval = tokio::time::interval(period);
    loop {
        tokio::select! {
            _ = interval.tick() => controller.tick().await,
            signal = tokio::signal::ctrl_c() => {
                signal?;
                break;
            }
        }
    }
    server.shutdown().await
}

#[cfg(test)]
mod tests {
    use aimedia_core::{ControlErrorCode, ControlRequest, PipelineConfig, PipelineMode};
    use aimedia_graph::compile as compile_plan;

    use super::{
        ControllerHandle, DriftCorrector, ProgramClock, RuntimePlanError, full_policy_name,
    };

    #[cfg(unix)]
    use super::{ControlServer, send_control_request};

    fn config() -> PipelineConfig {
        PipelineConfig::from_yaml(include_str!("../../../examples/director.yaml"))
            .expect("reference config parses")
    }

    fn single_config() -> PipelineConfig {
        PipelineConfig::from_yaml(include_str!("../../../examples/single-srt.yaml"))
            .expect("single-input config parses")
    }

    #[test]
    fn program_clock_is_monotonic_without_float_accumulation() {
        let mut clock = ProgramClock::new(30_000, 1_001);
        let points: Vec<u64> = (0..1_000).map(|_| clock.next_video_pts_90khz()).collect();
        assert!(points.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(points[1], 3_003);
    }

    #[test]
    fn drift_correction_is_limited_to_one_millisecond_per_second() {
        let mut corrector = DriftCorrector::new(0);
        assert_eq!(corrector.update(80, 1_000), -1_000);
        assert_eq!(corrector.update(80, 2_000), -2_000);
    }

    #[tokio::test]
    async fn rejects_take_to_unavailable_camera_before_switching() {
        let config = config();
        let controller = ControllerHandle::new(&config, false).expect("plan is executable");
        let response = controller
            .process(ControlRequest::take("req", "close", 5_000))
            .await;
        assert!(!response.accepted);
        assert_eq!(
            response.error_code,
            Some(ControlErrorCode::TargetUnavailable)
        );
        assert_eq!(response.state.expect("state is returned").active_input, 0);
    }

    #[tokio::test]
    async fn zero_hold_stays_manual_until_auto_is_requested() {
        let config = config();
        let controller = ControllerHandle::new(&config, true).expect("plan is executable");
        controller.tick().await;
        let taken = controller
            .process(ControlRequest::take("take", "close", 0))
            .await;
        assert!(taken.accepted);
        let taken_state = taken.state.expect("take state");
        assert_eq!(taken_state.mode, PipelineMode::Manual);
        assert_eq!(taken_state.last_reason, "Manual");

        let state = controller.process(ControlRequest::state("state")).await;
        let state = state.state.expect("current state");
        assert_eq!(state.mode, PipelineMode::Manual);
        assert_eq!(state.active_name, "close");

        let automatic = controller.process(ControlRequest::auto("auto")).await;
        assert_eq!(
            automatic.state.expect("auto state").mode,
            PipelineMode::Automatic
        );
    }

    #[tokio::test]
    async fn single_input_reports_mode_and_rejects_director_commands() {
        let config = single_config();
        let controller = ControllerHandle::new(&config, true).expect("plan is executable");

        let state = controller
            .process(ControlRequest::state("state"))
            .await
            .state
            .expect("single state");
        assert_eq!(state.mode, PipelineMode::Single);
        assert_eq!(state.inputs.len(), 1);
        assert_eq!(state.active_name, "program");
        assert!(!state.queues.is_empty());
        assert!(state.queues.iter().all(|queue| queue.capacity > 0));
        let serialized = serde_json::to_value(&state).expect("state serializes");
        assert_eq!(serialized["mode"], "single");
        assert_eq!(serialized["inputs"][0]["codec"]["videoDecodedFrames"], 0);
        assert_eq!(serialized["inputs"][0]["gpu"]["highWatermark"], 0);
        assert_eq!(serialized["output"]["srt"]["reconnects"], 0);
        assert!(serialized["queues"][0]["capacity"].as_u64().unwrap_or(0) > 0);

        for request in [
            ControlRequest::take("take", "program", 5_000),
            ControlRequest::auto("auto"),
        ] {
            let response = controller.process(request).await;
            assert!(!response.accepted);
            assert_eq!(response.error_code, Some(ControlErrorCode::NotApplicable));
            assert_eq!(
                response.state.expect("rejection state").mode,
                PipelineMode::Single
            );
        }
    }

    #[tokio::test]
    async fn controller_queue_state_comes_from_the_compiled_plan() {
        let config = single_config();
        let plan = compile_plan(&config).expect("plan compiles");
        let controller =
            ControllerHandle::from_plan(&config, &plan, true).expect("plan is executable");
        let state = controller.state().await;
        assert_eq!(state.queues.len(), plan.edges.len());
        for edge in &plan.edges {
            let queue = state
                .queues
                .iter()
                .find(|queue| queue.from == edge.from && queue.to == edge.to)
                .unwrap_or_else(|| panic!("queue {} -> {} is reported", edge.from, edge.to));
            assert_eq!(queue.capacity, edge.queue.capacity);
            assert_eq!(queue.full_policy, full_policy_name(edge.queue.full_policy));
        }
        let shared_output_edges = state
            .queues
            .iter()
            .filter(|queue| queue.name == "program.output")
            .count();
        assert_eq!(shared_output_edges, 3);
    }

    #[test]
    fn controller_rejects_a_plan_with_a_missing_runtime_edge() {
        let config = single_config();
        let mut plan = compile_plan(&config).expect("plan compiles");
        plan.edges
            .retain(|edge| edge.from != "input.0" || edge.to != "demux.0");
        assert!(matches!(
            ControllerHandle::from_plan(&config, &plan, true),
            Err(RuntimePlanError::MissingEdge { from, to })
                if from == "input.0" && to == "demux.0"
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_socket_round_trips_state_and_cleans_up() {
        use tokio::{
            io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
            net::UnixStream,
        };

        let config = config();
        let controller = ControllerHandle::new(&config, true).expect("plan is executable");
        let socket =
            std::env::temp_dir().join(format!("aimedia-control-{}.sock", std::process::id()));
        let server = ControlServer::start(&socket, "0600", controller)
            .await
            .expect("control server starts");
        let response = send_control_request(&socket, &ControlRequest::state("req-state"))
            .await
            .expect("state request succeeds");
        assert!(response.accepted);
        assert_eq!(response.request_id, "req-state");

        let mut stream = UnixStream::connect(&socket)
            .await
            .expect("second connection opens");
        let mut oversized = vec![b' '; 65_537];
        oversized.push(b'\n');
        stream
            .write_all(&oversized)
            .await
            .expect("oversized request is sent");
        let mut encoded_response = String::new();
        BufReader::new(stream)
            .read_line(&mut encoded_response)
            .await
            .expect("rejection is returned");
        let response: aimedia_core::ControlResponse =
            serde_json::from_str(&encoded_response).expect("rejection is JSON");
        assert_eq!(response.error_code, Some(ControlErrorCode::InvalidRequest));

        server.shutdown().await.expect("server stops");
        assert!(!socket.exists());
    }
}
