use aimedia_aac::Libxaac;
use aimedia_core::PipelineConfig;
use aimedia_graph::QueueCapacities;
use aimedia_nvidia::{NvdecConfig, NvdecDecoder, NvencConfig, NvencEncoder};
use aimedia_runtime::{
    ControlServer,
    single::{SinglePipeline, SinglePipelineBackends},
};
use aimedia_srt::{Endpoint, SrtTransport};
use anyhow::{Context, Result, anyhow, bail};

pub async fn run(config: PipelineConfig) -> Result<()> {
    let backends = build_backends(&config).await?;
    let pipeline = SinglePipeline::new(config.clone(), backends)
        .context("native single-input pipeline preflight failed")?;
    let controller = pipeline.controller();
    let server = ControlServer::start(
        &config.control.socket_path,
        &config.control.socket_mode,
        controller,
    )
    .await
    .context("could not start the local control socket")?;

    tracing::info!(
        pipeline = %config.metadata.name,
        input = %config.inputs[0].name,
        socket = %config.control.socket_path.display(),
        "native single-input media pipeline started"
    );

    let mut task = tokio::spawn(pipeline.run());
    let result = tokio::select! {
        joined = &mut task => match joined {
            Ok(result) => result
                .context("native single-input media pipeline failed")
                .map(|_| ()),
            Err(error) => Err(anyhow!("native media task failed to join: {error}")),
        },
        signal = tokio::signal::ctrl_c() => {
            task.abort();
            let _ = task.await;
            signal
                .context("failed to listen for the shutdown signal")
                .map(|_| ())
        }
    };

    let shutdown = server
        .shutdown()
        .await
        .context("could not remove the local control socket");
    result?;
    shutdown
}

async fn build_backends(config: &PipelineConfig) -> Result<SinglePipelineBackends> {
    let [input] = config.inputs.as_slice() else {
        bail!(
            "nativeSingleInputRequired: the native v0.2 runner requires exactly one input, got {}",
            config.inputs.len()
        );
    };

    let video = &config.media.video;
    let audio = &config.media.audio;
    Libxaac::validate_profile(audio.sample_rate, audio.channels, audio.bitrate_kbps)
        .context("configured audio profile is not supported by the native AAC backend")?;

    let aac = Libxaac::load().context("could not initialize the native AAC backend")?;
    let audio_decoder = aac
        .audio_decoder()
        .context("could not create the native AAC decoder")?;
    let audio_encoder = aac
        .audio_encoder()
        .context("could not create the native AAC encoder")?;

    let queue_capacities = QueueCapacities::from_config(config);
    let output_surfaces = u32::try_from(queue_capacities.video_frames.saturating_add(2))
        .context("video queue capacity exceeds the NVDEC surface counter")?;
    let video_decoder = NvdecDecoder::new(NvdecConfig {
        max_coded_width: align_up(video.width, 16),
        max_coded_height: align_up(video.height, 16),
        max_display_width: video.width,
        max_display_height: video.height,
        max_fps: video.fps,
        output_surfaces,
        ..NvdecConfig::default()
    })
    .context("could not initialize the NVDEC H.264 decoder")?;

    let gop_frames = frames_for_duration(video.gop_ms, video.fps);
    let bitrate = video
        .bitrate_kbps
        .checked_mul(1_000)
        .ok_or_else(|| anyhow!("media.video.bitrateKbps is too large"))?;
    let video_encoder = NvencEncoder::new(NvencConfig {
        width: video.width,
        height: video.height,
        fps_numerator: video.fps,
        bitrate,
        gop_frames,
        ..NvencConfig::default()
    })
    .context("could not initialize the NVENC H.264 encoder")?;

    let input_endpoint = Endpoint::from_config(&input.uri, &input.srt, input.secret_ref.as_ref())
        .context("could not prepare the SRT input")?;
    let output_endpoint = Endpoint::from_config(
        &config.output.uri,
        &config.output.srt,
        config.output.secret_ref.as_ref(),
    )
    .context("could not prepare the SRT output")?;

    let (input_transport, output_transport) = tokio::try_join!(
        SrtTransport::connect(input_endpoint),
        SrtTransport::connect(output_endpoint),
    )
    .context("could not establish the initial SRT input and output connections")?;

    Ok(SinglePipelineBackends {
        input: Box::new(input_transport),
        output: Box::new(output_transport),
        video_decoder: Box::new(video_decoder),
        video_encoder: Box::new(video_encoder),
        audio_decoder: Box::new(audio_decoder),
        audio_encoder: Box::new(audio_encoder),
    })
}

const fn align_up(value: u32, alignment: u32) -> u32 {
    value.saturating_add(alignment - 1) / alignment * alignment
}

fn frames_for_duration(duration_ms: u64, fps: u32) -> u32 {
    let frames = duration_ms
        .saturating_mul(u64::from(fps))
        .saturating_add(999)
        / 1_000;
    u32::try_from(frames).unwrap_or(u32::MAX).max(1)
}
