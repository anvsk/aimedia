//! Bounded single-input media scheduler.
//!
//! Production codecs are injected through the core backend traits. This keeps the scheduler
//! testable on CPU-only contributors' machines and does not imply a CPU fallback in production.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use aimedia_core::{
    PipelineConfig, PipelineRuntimeState, Timestamp,
    backend::{
        AudioDecoder, AudioEncoder, AudioFrame, BackendError, CodecId, GpuSurfaceObserver,
        MediaPacket, Transport, TransportChunk, TransportObserver, VideoDecoder, VideoEncoder,
        VideoFrame,
    },
};
use aimedia_graph::{ExecutionPlan, FullPolicy, JobMode, compile as compile_plan};
use aimedia_mpegts::{DemuxEvent, MuxPacket, MuxStream, StreamDemuxer, StreamMuxer, TsError};
use thiserror::Error;
use tokio::{
    sync::{Mutex, mpsc, watch},
    task::JoinSet,
};

use crate::{
    ControllerHandle, ProgramClock, RuntimePlanError, plan_queue_capacity, validate_runtime_plan,
};

#[derive(Debug, Error)]
pub enum SinglePipelineError {
    #[error("single pipeline requires exactly one configured input, got {0}")]
    InputCount(usize),
    #[error("media backend failed: {0}")]
    Backend(#[from] BackendError),
    #[error("MPEG-TS processing failed: {0}")]
    MpegTs(#[from] TsError),
    #[error("bounded channel {0:?} closed before its producer")]
    ChannelClosed(String),
    #[error("pipeline task {task:?} failed to join: {message}")]
    Join { task: &'static str, message: String },
    #[error("encoded packet has unsupported codec {0:?}")]
    EncodedCodec(CodecId),
    #[error("audio frame has invalid sample layout")]
    InvalidAudioFrame,
    #[error("execution plan failed preflight: {0}")]
    Plan(#[from] RuntimePlanError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SingleQueuePlan {
    transport_messages: usize,
    video_packets: usize,
    audio_packets: usize,
    video_frames: usize,
    audio_frames: usize,
    encoded_messages: usize,
}

impl SingleQueuePlan {
    fn from_plan(plan: &ExecutionPlan) -> Result<Self, RuntimePlanError> {
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
        if video_output != audio_output {
            return Err(RuntimePlanError::Invariant(format!(
                "the shared encoded queue requires equal video/audio capacities, got {video_output} and {audio_output}"
            )));
        }
        let video_frames = plan_queue_capacity(
            plan,
            "video.decode.0",
            "video.timeline",
            FullPolicy::KeepLatest,
        )?;
        if video_frames != 1 {
            return Err(RuntimePlanError::Invariant(format!(
                "the latest-video slot requires capacity 1, got {video_frames}"
            )));
        }
        Ok(Self {
            transport_messages: plan_queue_capacity(
                plan,
                "input.0",
                "demux.0",
                FullPolicy::Backpressure,
            )?,
            video_packets: plan_queue_capacity(
                plan,
                "demux.0",
                "video.decode.0",
                FullPolicy::Backpressure,
            )?,
            audio_packets: plan_queue_capacity(
                plan,
                "demux.0",
                "audio.decode.0",
                FullPolicy::Backpressure,
            )?,
            video_frames,
            audio_frames: plan_queue_capacity(
                plan,
                "audio.decode.0",
                "audio.timeline",
                FullPolicy::Backpressure,
            )?,
            encoded_messages: video_output,
        })
    }
}

// Live video is sampled on the program clock. Retaining older decoded frames only increases
// latency and pins scarce NVDEC mappings, so the timeline owns exactly one replaceable frame.
struct LatestVideoSlot {
    frame: Mutex<Option<VideoFrame>>,
    sender_open: AtomicBool,
    receiver_open: AtomicBool,
}

struct LatestVideoSender {
    slot: Arc<LatestVideoSlot>,
}

struct LatestVideoReceiver {
    slot: Arc<LatestVideoSlot>,
}

fn latest_video_slot(capacity: usize) -> (LatestVideoSender, LatestVideoReceiver) {
    debug_assert_eq!(capacity, 1);
    let slot = Arc::new(LatestVideoSlot {
        frame: Mutex::new(None),
        sender_open: AtomicBool::new(true),
        receiver_open: AtomicBool::new(true),
    });
    (
        LatestVideoSender {
            slot: Arc::clone(&slot),
        },
        LatestVideoReceiver { slot },
    )
}

impl LatestVideoSender {
    async fn replace(&self, frame: VideoFrame) -> Result<bool, ()> {
        if !self.slot.receiver_open.load(Ordering::Acquire) {
            return Err(());
        }
        let mut current = self.slot.frame.lock().await;
        if !self.slot.receiver_open.load(Ordering::Acquire) {
            return Err(());
        }
        Ok(current.replace(frame).is_some())
    }
}

impl Drop for LatestVideoSender {
    fn drop(&mut self) {
        self.slot.sender_open.store(false, Ordering::Release);
    }
}

impl LatestVideoReceiver {
    async fn take(&self) -> (Option<VideoFrame>, bool) {
        let frame = self.slot.frame.lock().await.take();
        let sender_open = self.slot.sender_open.load(Ordering::Acquire);
        (frame, sender_open)
    }
}

impl Drop for LatestVideoReceiver {
    fn drop(&mut self) {
        self.slot.receiver_open.store(false, Ordering::Release);
    }
}

pub struct SinglePipelineBackends {
    pub input: Box<dyn Transport>,
    pub output: Box<dyn Transport>,
    pub video_decoder: Box<dyn VideoDecoder>,
    pub video_encoder: Box<dyn VideoEncoder>,
    pub audio_decoder: Box<dyn AudioDecoder>,
    pub audio_encoder: Box<dyn AudioEncoder>,
}

impl std::fmt::Debug for SinglePipelineBackends {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("SinglePipelineBackends").finish()
    }
}

#[derive(Debug)]
pub struct SinglePipeline {
    config: PipelineConfig,
    plan: ExecutionPlan,
    controller: ControllerHandle,
    backends: SinglePipelineBackends,
}

impl SinglePipeline {
    pub fn new(
        config: PipelineConfig,
        backends: SinglePipelineBackends,
    ) -> Result<Self, SinglePipelineError> {
        if config.inputs.len() != 1 {
            return Err(SinglePipelineError::InputCount(config.inputs.len()));
        }
        let plan = compile_plan(&config).map_err(RuntimePlanError::from)?;
        validate_runtime_plan(&config, &plan)?;
        if plan.mode != JobMode::Single {
            return Err(RuntimePlanError::Invariant(format!(
                "single executor received {:?} plan",
                plan.mode
            ))
            .into());
        }
        SingleQueuePlan::from_plan(&plan)?;
        let controller = ControllerHandle::from_plan(&config, &plan, true)?;
        Ok(Self {
            config,
            plan,
            controller,
            backends,
        })
    }

    #[must_use]
    pub fn controller(&self) -> ControllerHandle {
        self.controller.clone()
    }

    #[must_use]
    pub const fn plan(&self) -> &ExecutionPlan {
        &self.plan
    }

    pub async fn run(self) -> Result<PipelineRuntimeState, SinglePipelineError> {
        let Self {
            config,
            plan,
            controller,
            backends,
        } = self;
        let SinglePipelineBackends {
            input,
            output,
            video_decoder,
            video_encoder,
            audio_decoder,
            audio_encoder,
        } = backends;
        let input_observer = input.observer();
        let output_observer = output.observer();
        let surface_observer = video_decoder.surface_observer();
        let queues = SingleQueuePlan::from_plan(&plan)?;
        let input_name = config.inputs[0].name.clone();
        let video_period = Duration::from_secs_f64(1.0 / f64::from(config.media.video.fps));
        let audio_period =
            Duration::from_secs_f64(1_024.0 / f64::from(config.media.audio.sample_rate));
        let clock = Arc::new(Mutex::new(ProgramClock::new(
            u64::from(config.media.video.fps),
            1,
        )));

        let (transport_tx, transport_rx) = mpsc::channel(queues.transport_messages);
        let (video_packet_tx, video_packet_rx) = mpsc::channel(queues.video_packets);
        let (audio_packet_tx, audio_packet_rx) = mpsc::channel(queues.audio_packets);
        let (video_frame_tx, video_frame_rx) = latest_video_slot(queues.video_frames);
        let (audio_frame_tx, audio_frame_rx) = mpsc::channel(queues.audio_frames);
        let (encoded_tx, encoded_rx) = mpsc::channel(queues.encoded_messages);
        let request_idr = Arc::new(AtomicBool::new(true));
        let mut tasks = JoinSet::new();
        let mut monitors = JoinSet::new();
        let (monitor_stop, monitor_stop_rx) = watch::channel(false);

        if let Some(observer) = input_observer {
            monitors.spawn(monitor_transport(
                observer,
                TransportStateTarget::Input(0),
                controller.clone(),
                monitor_stop_rx.clone(),
            ));
        }
        if let Some(observer) = output_observer {
            monitors.spawn(monitor_transport(
                observer,
                TransportStateTarget::Output,
                controller.clone(),
                monitor_stop_rx.clone(),
            ));
        }
        if let Some(observer) = surface_observer {
            monitors.spawn(monitor_gpu_surfaces(
                observer,
                0,
                controller.clone(),
                monitor_stop_rx,
            ));
        }

        let receive_controller = controller.clone();
        let receive_queue = format!("{input_name}.transport");
        tasks.spawn(async move {
            (
                "receive",
                receive_transport(input, transport_tx, receive_controller, receive_queue).await,
            )
        });
        let demux_controller = controller.clone();
        let demux_transport_queue = format!("{input_name}.transport");
        let demux_video_queue = format!("{input_name}.videoDecode");
        let demux_audio_queue = format!("{input_name}.audioDecode");
        tasks.spawn(async move {
            (
                "demux",
                demux_transport(
                    transport_rx,
                    video_packet_tx,
                    audio_packet_tx,
                    demux_controller,
                    demux_transport_queue,
                    demux_video_queue,
                    demux_audio_queue,
                )
                .await,
            )
        });
        let video_decode_controller = controller.clone();
        let video_decode_input_queue = format!("{input_name}.videoDecode");
        let video_decode_output_queue = format!("{input_name}.videoTimeline");
        tasks.spawn(async move {
            (
                "videoDecode",
                decode_video(
                    video_decoder,
                    video_packet_rx,
                    video_frame_tx,
                    video_decode_controller,
                    video_decode_input_queue,
                    video_decode_output_queue,
                )
                .await,
            )
        });
        let audio_decode_controller = controller.clone();
        let audio_decode_input_queue = format!("{input_name}.audioDecode");
        let audio_decode_output_queue = format!("{input_name}.audioTimeline");
        tasks.spawn(async move {
            (
                "audioDecode",
                decode_audio(
                    audio_decoder,
                    audio_packet_rx,
                    audio_frame_tx,
                    audio_decode_controller,
                    audio_decode_input_queue,
                    audio_decode_output_queue,
                )
                .await,
            )
        });
        let video_encode_controller = controller.clone();
        let video_encode_input_queue = format!("{input_name}.videoTimeline");
        let video_request_idr = Arc::clone(&request_idr);
        let video_clock = Arc::clone(&clock);
        let video_encoded_tx = encoded_tx.clone();
        tasks.spawn(async move {
            (
                "videoEncode",
                encode_video(
                    video_encoder,
                    video_frame_rx,
                    video_encoded_tx,
                    video_clock,
                    video_request_idr,
                    video_period,
                    video_encode_controller,
                    video_encode_input_queue,
                    "program.output".to_owned(),
                )
                .await,
            )
        });
        let audio_encode_controller = controller.clone();
        let audio_encode_input_queue = format!("{input_name}.audioTimeline");
        tasks.spawn(async move {
            (
                "audioEncode",
                encode_audio(
                    audio_encoder,
                    audio_frame_rx,
                    encoded_tx,
                    clock,
                    audio_period,
                    config.media.audio.sample_rate,
                    config.media.audio.channels,
                    audio_encode_controller,
                    audio_encode_input_queue,
                    "program.output".to_owned(),
                )
                .await,
            )
        });
        let output_controller = controller.clone();
        tasks.spawn(async move {
            (
                "output",
                mux_and_send(output, encoded_rx, request_idr, output_controller).await,
            )
        });

        let mut completed = 0_usize;
        let failure = loop {
            match tasks.join_next().await {
                Some(Ok((_name, Ok(())))) => {
                    completed += 1;
                    if completed == 7 {
                        break None;
                    }
                }
                Some(Ok((_name, Err(error)))) => break Some(error),
                Some(Err(error)) => {
                    break Some(SinglePipelineError::Join {
                        task: "pipeline",
                        message: error.to_string(),
                    });
                }
                None => break None,
            }
        };
        if let Some(error) = failure {
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
            let _ = monitor_stop.send(true);
            while monitors.join_next().await.is_some() {}
            controller.finish().await;
            return Err(error);
        }

        let _ = monitor_stop.send(true);
        while monitors.join_next().await.is_some() {}
        controller.finish().await;
        Ok(controller.state().await)
    }
}

async fn send_observed<T>(
    sender: &mpsc::Sender<T>,
    value: T,
    controller: &ControllerHandle,
    queue: &str,
) -> Result<(), SinglePipelineError> {
    let observed_depth = (sender.max_capacity() - sender.capacity())
        .saturating_add(1)
        .min(sender.max_capacity());
    sender
        .send(value)
        .await
        .map_err(|_| SinglePipelineError::ChannelClosed(queue.to_owned()))?;
    controller.observe_queue(queue, observed_depth).await;
    Ok(())
}

async fn send_latest_video(
    sender: &LatestVideoSender,
    frame: VideoFrame,
    controller: &ControllerHandle,
    queue: &str,
) -> Result<(), SinglePipelineError> {
    let replaced = sender
        .replace(frame)
        .await
        .map_err(|()| SinglePipelineError::ChannelClosed(queue.to_owned()))?;
    if replaced {
        controller.record_input_drop(CodecId::H264).await;
    }
    controller.observe_queue(queue, 1).await;
    Ok(())
}

async fn receive_transport(
    mut input: Box<dyn Transport>,
    sender: mpsc::Sender<TransportChunk>,
    controller: ControllerHandle,
    queue: String,
) -> Result<(), SinglePipelineError> {
    loop {
        match input.receive().await {
            Ok(payload) => send_observed(&sender, payload, &controller, &queue).await?,
            Err(BackendError::EndOfStream) => break,
            Err(error) => return Err(error.into()),
        }
    }
    input.close().await?;
    Ok(())
}

async fn demux_transport(
    mut receiver: mpsc::Receiver<TransportChunk>,
    video_sender: mpsc::Sender<MediaPacket>,
    audio_sender: mpsc::Sender<MediaPacket>,
    controller: ControllerHandle,
    input_queue: String,
    video_queue: String,
    audio_queue: String,
) -> Result<(), SinglePipelineError> {
    let mut demuxer = StreamDemuxer::new();
    let mut pending_discontinuity = [false; 2];
    while let Some(chunk) = receiver.recv().await {
        controller.observe_queue(&input_queue, receiver.len()).await;
        if chunk.discontinuity {
            demuxer = StreamDemuxer::new();
            pending_discontinuity = [true; 2];
            controller.mark_input_discontinuity(0).await;
            tracing::warn!("input transport reconnected; reset MPEG-TS state");
        }
        route_demux_events(
            demuxer.push(&chunk.data)?,
            &video_sender,
            &audio_sender,
            &controller,
            &video_queue,
            &audio_queue,
            &mut pending_discontinuity,
        )
        .await?;
    }
    route_demux_events(
        demuxer.flush()?,
        &video_sender,
        &audio_sender,
        &controller,
        &video_queue,
        &audio_queue,
        &mut pending_discontinuity,
    )
    .await
}

#[derive(Debug, Clone, Copy)]
enum TransportStateTarget {
    Input(usize),
    Output,
}

async fn monitor_transport(
    observer: Arc<dyn TransportObserver>,
    target: TransportStateTarget,
    controller: ControllerHandle,
    mut stop: watch::Receiver<bool>,
) {
    let mut interval = tokio::time::interval(Duration::from_millis(250));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    break;
                }
            }
            _ = interval.tick() => match observer.stats().await {
                Ok(stats) => match target {
                    TransportStateTarget::Input(index) => {
                        controller.set_input_srt(index, stats).await;
                    }
                    TransportStateTarget::Output => controller.set_output_srt(stats).await,
                },
                Err(error) => {
                    tracing::warn!(?target, %error, "could not sample transport state");
                }
            }
        }
    }
}

async fn monitor_gpu_surfaces(
    observer: Arc<dyn GpuSurfaceObserver>,
    input: usize,
    controller: ControllerHandle,
    mut stop: watch::Receiver<bool>,
) {
    let mut interval = tokio::time::interval(Duration::from_millis(100));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    break;
                }
            }
            _ = interval.tick() => {
                controller.set_gpu_surfaces(input, observer.stats()).await;
            }
        }
    }
}

async fn route_demux_events(
    events: Vec<DemuxEvent>,
    video_sender: &mpsc::Sender<MediaPacket>,
    audio_sender: &mpsc::Sender<MediaPacket>,
    controller: &ControllerHandle,
    video_queue: &str,
    audio_queue: &str,
    pending_discontinuity: &mut [bool; 2],
) -> Result<(), SinglePipelineError> {
    for event in events {
        match event {
            DemuxEvent::Packet(packet) => {
                let (codec, sender, queue, stream_index) = match packet.stream {
                    MuxStream::Video => (CodecId::H264, video_sender, video_queue, 0),
                    MuxStream::Audio => (CodecId::AacLc, audio_sender, audio_queue, 1),
                };
                let discontinuity = packet.discontinuity
                    || std::mem::take(&mut pending_discontinuity[stream_index]);
                let packet = MediaPacket {
                    stream_id: u32::from(packet.pid),
                    codec,
                    pts: timestamp_90khz(packet.pts_90khz),
                    dts: packet.dts_90khz.map(timestamp_90khz),
                    duration: None,
                    keyframe: packet.keyframe,
                    discontinuity,
                    data: packet.data,
                };
                send_observed(sender, packet, controller, queue).await?;
            }
            DemuxEvent::ContinuityError { pid, .. } | DemuxEvent::Discontinuity { pid } => {
                tracing::warn!(pid, "input MPEG-TS discontinuity");
            }
            DemuxEvent::SyncRecovered { discarded_bytes } => {
                tracing::warn!(discarded_bytes, "input MPEG-TS sync recovered");
            }
            DemuxEvent::CorruptData { pid, reason } => {
                tracing::warn!(?pid, %reason, "discarded corrupt MPEG-TS data");
            }
            DemuxEvent::ProgramMap(_) => {}
        }
    }
    Ok(())
}

async fn decode_video(
    mut decoder: Box<dyn VideoDecoder>,
    mut receiver: mpsc::Receiver<MediaPacket>,
    sender: LatestVideoSender,
    controller: ControllerHandle,
    input_queue: String,
    output_queue: String,
) -> Result<(), SinglePipelineError> {
    let mut waiting_for_idr = true;
    while let Some(packet) = receiver.recv().await {
        controller.observe_queue(&input_queue, receiver.len()).await;
        if packet.discontinuity {
            waiting_for_idr = true;
        }
        if waiting_for_idr && !packet.keyframe {
            controller.record_input_drop(CodecId::H264).await;
            continue;
        }
        waiting_for_idr = false;
        let frames = decoder.decode(packet).await?;
        controller.record_decoded(CodecId::H264, frames.len()).await;
        for frame in frames {
            send_latest_video(&sender, frame, &controller, &output_queue).await?;
        }
    }
    let frames = decoder.flush().await?;
    controller.record_decoded(CodecId::H264, frames.len()).await;
    for frame in frames {
        send_latest_video(&sender, frame, &controller, &output_queue).await?;
    }
    Ok(())
}

async fn decode_audio(
    mut decoder: Box<dyn AudioDecoder>,
    mut receiver: mpsc::Receiver<MediaPacket>,
    sender: mpsc::Sender<AudioFrame>,
    controller: ControllerHandle,
    input_queue: String,
    output_queue: String,
) -> Result<(), SinglePipelineError> {
    while let Some(packet) = receiver.recv().await {
        controller.observe_queue(&input_queue, receiver.len()).await;
        let frames = decoder.decode(packet).await?;
        controller
            .record_decoded(CodecId::AacLc, frames.len())
            .await;
        for frame in frames {
            send_observed(&sender, frame, &controller, &output_queue).await?;
        }
    }
    let frames = decoder.flush().await?;
    controller
        .record_decoded(CodecId::AacLc, frames.len())
        .await;
    for frame in frames {
        send_observed(&sender, frame, &controller, &output_queue).await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn encode_video(
    mut encoder: Box<dyn VideoEncoder>,
    receiver: LatestVideoReceiver,
    sender: mpsc::Sender<MediaPacket>,
    clock: Arc<Mutex<ProgramClock>>,
    request_idr: Arc<AtomicBool>,
    frame_period: Duration,
    controller: ControllerHandle,
    input_queue: String,
    output_queue: String,
) -> Result<(), SinglePipelineError> {
    let mut last_frame: Option<VideoFrame> = None;
    let mut ticker =
        tokio::time::interval_at(tokio::time::Instant::now() + frame_period, frame_period);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let (frame, sender_open) = receiver.take().await;
        if let Some(frame) = frame {
            last_frame = Some(frame);
            controller.observe_queue(&input_queue, 0).await;
        } else if !sender_open {
            break;
        }
        let Some(mut frame) = last_frame.clone() else {
            continue;
        };
        let pts = clock.lock().await.next_video_pts_90khz();
        frame.pts = timestamp_90khz(pts);
        let force_idr = request_idr.swap(false, Ordering::AcqRel);
        let packets = encoder.encode(frame, force_idr).await?;
        for mut packet in packets {
            packet.codec = CodecId::H264;
            packet.pts = timestamp_90khz(pts);
            packet.dts = Some(timestamp_90khz(pts));
            packet.keyframe |= pts == 0;
            controller.record_encoded(CodecId::H264).await;
            send_observed(&sender, packet, &controller, &output_queue).await?;
        }
    }
    for packet in encoder.flush().await? {
        controller.record_encoded(CodecId::H264).await;
        send_observed(&sender, packet, &controller, &output_queue).await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn encode_audio(
    mut encoder: Box<dyn AudioEncoder>,
    mut receiver: mpsc::Receiver<AudioFrame>,
    sender: mpsc::Sender<MediaPacket>,
    clock: Arc<Mutex<ProgramClock>>,
    block_period: Duration,
    sample_rate: u32,
    channels: u8,
    controller: ControllerHandle,
    input_queue: String,
    output_queue: String,
) -> Result<(), SinglePipelineError> {
    let mut ticker =
        tokio::time::interval_at(tokio::time::Instant::now() + block_period, block_period);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let mut frame = match receiver.try_recv() {
            Ok(frame) => frame,
            Err(mpsc::error::TryRecvError::Empty) => AudioFrame {
                pts: timestamp_90khz(0),
                sample_rate,
                channels,
                interleaved: vec![0.0; 1_024 * usize::from(channels)],
            },
            Err(mpsc::error::TryRecvError::Disconnected) => break,
        };
        controller.observe_queue(&input_queue, receiver.len()).await;
        let frame_channels = usize::from(frame.channels);
        if frame_channels == 0 || frame.interleaved.len() % frame_channels != 0 {
            return Err(SinglePipelineError::InvalidAudioFrame);
        }
        let samples = u32::try_from(frame.interleaved.len() / frame_channels)
            .map_err(|_| SinglePipelineError::InvalidAudioFrame)?;
        let pts = clock
            .lock()
            .await
            .next_audio_pts_90khz(samples, frame.sample_rate);
        frame.pts = timestamp_90khz(pts);
        let packets = encoder.encode(frame).await?;
        for mut packet in packets {
            packet.codec = CodecId::AacLc;
            packet.pts = timestamp_90khz(pts);
            packet.dts = Some(timestamp_90khz(pts));
            controller.record_encoded(CodecId::AacLc).await;
            send_observed(&sender, packet, &controller, &output_queue).await?;
        }
    }
    for packet in encoder.flush().await? {
        controller.record_encoded(CodecId::AacLc).await;
        send_observed(&sender, packet, &controller, &output_queue).await?;
    }
    Ok(())
}

async fn mux_and_send(
    mut output: Box<dyn Transport>,
    mut receiver: mpsc::Receiver<MediaPacket>,
    request_idr: Arc<AtomicBool>,
    controller: ControllerHandle,
) -> Result<(), SinglePipelineError> {
    let queue = "program.output";
    let mut muxer = StreamMuxer::new();
    let mut output_failed = false;
    let mut last_warning = None;
    while let Some(mut packet) = receiver.recv().await {
        if output_failed {
            while let Ok(next) = receiver.try_recv() {
                controller.record_output_drop(packet.codec).await;
                packet = next;
            }
            request_idr.store(true, Ordering::Release);
        }
        controller.observe_queue(queue, receiver.len()).await;
        let stream = match packet.codec {
            CodecId::H264 => MuxStream::Video,
            CodecId::AacLc => MuxStream::Audio,
            codec => return Err(SinglePipelineError::EncodedCodec(codec)),
        };
        let muxed = muxer.push(&MuxPacket {
            stream,
            pts_90khz: to_90khz(packet.pts),
            dts_90khz: packet.dts.map(to_90khz),
            keyframe: packet.keyframe,
            data: packet.data,
        })?;
        match output.send(&muxed).await {
            Ok(()) => {
                if output_failed {
                    tracing::info!(
                        "output transport reconnected; resumed with fresh program tables"
                    );
                    output_failed = false;
                    last_warning = None;
                }
            }
            Err(error @ BackendError::Io(_)) => {
                controller.record_output_drop(packet.codec).await;
                request_idr.store(true, Ordering::Release);
                muxer = StreamMuxer::new();
                output_failed = true;
                let now = Instant::now();
                let should_warn = last_warning
                    .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(1));
                if should_warn {
                    tracing::warn!(%error, "dropping live output while transport reconnects");
                    last_warning = Some(now);
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    output.close().await?;
    Ok(())
}

fn timestamp_90khz(value: u64) -> Timestamp {
    Timestamp::new(
        i64::try_from(value).unwrap_or(i64::MAX),
        Timestamp::MPEG_TS_TIMESCALE,
    )
}

fn to_90khz(timestamp: Timestamp) -> u64 {
    let ticks = i128::from(timestamp.ticks) * i128::from(Timestamp::MPEG_TS_TIMESCALE)
        / i128::from(timestamp.timescale);
    ticks.clamp(0, i128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex as StdMutex},
    };

    use aimedia_core::{
        PipelineConfig, Timestamp,
        backend::{
            AudioDecoder, AudioEncoder, AudioFrame, BackendError, CodecId, MediaPacket,
            MemoryDomain, PixelFormat, SurfaceLease, Transport, TransportChunk, VideoDecoder,
            VideoEncoder, VideoFrame, VideoSurface,
        },
    };
    use aimedia_mpegts::{DemuxEvent, MuxPacket, MuxStream, StreamDemuxer, StreamMuxer};
    use async_trait::async_trait;
    use bytes::Bytes;

    use super::{SinglePipeline, SinglePipelineBackends};

    #[derive(Debug)]
    struct FakeTransport {
        receive: VecDeque<Vec<u8>>,
        sent: Arc<StdMutex<Vec<Vec<u8>>>>,
    }

    #[async_trait]
    impl Transport for FakeTransport {
        async fn receive(&mut self) -> Result<TransportChunk, BackendError> {
            self.receive
                .pop_front()
                .map(|data| TransportChunk {
                    data,
                    discontinuity: false,
                })
                .ok_or(BackendError::EndOfStream)
        }

        async fn send(&mut self, payload: &[u8]) -> Result<(), BackendError> {
            self.sent.lock().expect("sent lock").push(payload.to_vec());
            Ok(())
        }

        async fn close(&mut self) -> Result<(), BackendError> {
            Ok(())
        }
    }

    #[derive(Debug)]
    struct FakeLease(u64);

    impl SurfaceLease for FakeLease {
        fn handle(&self) -> u64 {
            self.0
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    #[derive(Debug, Default)]
    struct FakeVideoDecoder;

    #[async_trait]
    impl VideoDecoder for FakeVideoDecoder {
        async fn decode(&mut self, packet: MediaPacket) -> Result<Vec<VideoFrame>, BackendError> {
            Ok(vec![VideoFrame {
                pts: packet.pts,
                width: 1_920,
                height: 1_080,
                format: PixelFormat::Nv12,
                memory: MemoryDomain::Cuda { device: 0 },
                surface: VideoSurface::new(FakeLease(u64::from(packet.stream_id))),
            }])
        }

        async fn flush(&mut self) -> Result<Vec<VideoFrame>, BackendError> {
            Ok(Vec::new())
        }
    }

    #[derive(Debug, Default)]
    struct FakeVideoEncoder;

    #[async_trait]
    impl VideoEncoder for FakeVideoEncoder {
        async fn encode(
            &mut self,
            frame: VideoFrame,
            force_idr: bool,
        ) -> Result<Vec<MediaPacket>, BackendError> {
            Ok(vec![MediaPacket {
                stream_id: 0,
                codec: CodecId::H264,
                pts: frame.pts,
                dts: Some(frame.pts),
                duration: None,
                keyframe: force_idr,
                discontinuity: false,
                data: Bytes::from_static(&[0, 0, 0, 1, 0x65, 0x88]),
            }])
        }

        async fn flush(&mut self) -> Result<Vec<MediaPacket>, BackendError> {
            Ok(Vec::new())
        }
    }

    #[derive(Debug, Default)]
    struct FakeAudioDecoder;

    #[async_trait]
    impl AudioDecoder for FakeAudioDecoder {
        async fn decode(&mut self, packet: MediaPacket) -> Result<Vec<AudioFrame>, BackendError> {
            Ok(vec![AudioFrame {
                pts: packet.pts,
                sample_rate: 48_000,
                channels: 2,
                interleaved: vec![0.0; 2_048],
            }])
        }
    }

    #[derive(Debug, Default)]
    struct FakeAudioEncoder;

    #[async_trait]
    impl AudioEncoder for FakeAudioEncoder {
        async fn encode(&mut self, frame: AudioFrame) -> Result<Vec<MediaPacket>, BackendError> {
            Ok(vec![MediaPacket {
                stream_id: 1,
                codec: CodecId::AacLc,
                pts: frame.pts,
                dts: Some(frame.pts),
                duration: None,
                keyframe: true,
                discontinuity: false,
                data: Bytes::from_static(&[
                    0xff, 0xf1, 0x4c, 0x80, 0x01, 0x7f, 0xfc, 0x11, 0x22, 0x33, 0x44,
                ]),
            }])
        }
    }

    fn input_transport() -> Vec<Vec<u8>> {
        let mut muxer = StreamMuxer::new();
        let mut bytes = Vec::new();
        for index in 0..2_u64 {
            bytes.extend(
                muxer
                    .push(&MuxPacket {
                        stream: MuxStream::Video,
                        pts_90khz: 450_000 + index * 3_000,
                        dts_90khz: None,
                        keyframe: index == 0,
                        data: Bytes::from_static(&[0, 0, 0, 1, 0x65, 0x88]),
                    })
                    .expect("video muxes"),
            );
            bytes.extend(
                muxer
                    .push(&MuxPacket {
                        stream: MuxStream::Audio,
                        pts_90khz: 450_000 + index * 1_920,
                        dts_90khz: None,
                        keyframe: true,
                        data: Bytes::from_static(&[
                            0xff, 0xf1, 0x4c, 0x80, 0x01, 0x7f, 0xfc, 0x11, 0x22, 0x33, 0x44,
                        ]),
                    })
                    .expect("audio muxes"),
            );
        }
        bytes.chunks(317).map(<[u8]>::to_vec).collect()
    }

    #[tokio::test]
    async fn bounded_single_pipeline_retimes_and_drains_every_queue() {
        let config = PipelineConfig::from_yaml(include_str!("../../../examples/single-srt.yaml"))
            .expect("single config");
        let sent = Arc::new(StdMutex::new(Vec::new()));
        let pipeline = SinglePipeline::new(
            config,
            SinglePipelineBackends {
                input: Box::new(FakeTransport {
                    receive: input_transport().into(),
                    sent: Arc::new(StdMutex::new(Vec::new())),
                }),
                output: Box::new(FakeTransport {
                    receive: VecDeque::new(),
                    sent: Arc::clone(&sent),
                }),
                video_decoder: Box::new(FakeVideoDecoder),
                video_encoder: Box::new(FakeVideoEncoder),
                audio_decoder: Box::new(FakeAudioDecoder),
                audio_encoder: Box::new(FakeAudioEncoder),
            },
        )
        .expect("pipeline builds");
        assert_eq!(
            pipeline
                .plan()
                .queue("input.0", "demux.0")
                .expect("transport edge")
                .capacity,
            256
        );

        let state = pipeline.run().await.expect("pipeline completes");
        assert!(!state.running);
        assert_eq!(state.inputs[0].codec.video_decoded_frames, 2);
        assert_eq!(state.inputs[0].codec.video_dropped_frames, 1);
        assert_eq!(state.inputs[0].codec.audio_decoded_frames, 2);
        assert_eq!(state.output.video_encoded_frames, 1);
        assert_eq!(state.output.audio_encoded_frames, 2);
        assert!(state.queues.iter().all(|queue| queue.depth == 0));
        assert!(
            state
                .queues
                .iter()
                .all(|queue| queue.high_watermark <= queue.capacity)
        );
        assert!(state.queues.iter().any(|queue| queue.high_watermark > 0));

        let bytes: Vec<u8> = sent
            .lock()
            .expect("sent lock")
            .iter()
            .flatten()
            .copied()
            .collect();
        let mut demuxer = StreamDemuxer::new();
        let mut events = demuxer.push(&bytes).expect("output demuxes");
        events.extend(demuxer.flush().expect("output flushes"));
        let mut video_pts = Vec::new();
        let mut audio_pts = Vec::new();
        for event in events {
            if let DemuxEvent::Packet(packet) = event {
                match packet.stream {
                    MuxStream::Video => video_pts.push(packet.pts_90khz),
                    MuxStream::Audio => audio_pts.push(packet.pts_90khz),
                }
            }
        }
        assert_eq!(video_pts, vec![0]);
        assert_eq!(audio_pts, vec![0, 1_920]);
        assert!(video_pts.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(audio_pts.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(Timestamp::MPEG_TS_TIMESCALE, 90_000);
    }
}
