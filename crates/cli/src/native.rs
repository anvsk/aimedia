use aimedia_aac::Libxaac;
use aimedia_core::{
    PipelineConfig,
    backend::{AudioDecoder, PacketSource},
    config::RtspTransport,
};
use aimedia_nvidia::{NvdecConfig, NvdecDecoder, NvencConfig, NvencEncoder};
use aimedia_rtsp::{G711Decoder, RtspCodec, RtspEndpoint, RtspMediaProfile, RtspPacketSource};
use aimedia_runtime::{
    ControlServer,
    single::{SingleInput, SinglePipeline, SinglePipelineBackends},
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
    let audio_encoder = aac
        .audio_encoder()
        .context("could not create the native AAC encoder")?;

    // The program timeline retains one latest frame, so the fixed NVDEC default provides
    // bounded decoder headroom without scaling GPU surfaces with the network buffer duration.
    let video_decoder = NvdecDecoder::new(NvdecConfig {
        max_coded_width: align_up(video.width, 16),
        max_coded_height: align_up(video.height, 16),
        max_display_width: video.width,
        max_display_height: video.height,
        max_fps: video.fps,
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

    let output_endpoint = Endpoint::from_config(
        &config.output.uri,
        &config.output.srt,
        config.output.secret_ref.as_ref(),
    )
    .context("could not prepare the SRT output")?;

    let (input, audio_decoder): (SingleInput, Box<dyn AudioDecoder>) = if input
        .uri
        .get(..7)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("rtsp://"))
    {
        let rtsp = input
            .rtsp
            .as_ref()
            .ok_or_else(|| anyhow!("rtspConfigRequired: RTSP input requires inputs[0].rtsp"))?;
        if rtsp.transport != RtspTransport::Tcp {
            bail!(
                "rtspUdpPendingV3_02D: the native runtime currently supports RTSP interleaved TCP only"
            );
        }
        let endpoint = RtspEndpoint::from_config(&input.uri, rtsp)
            .context("could not prepare the RTSP input")?;
        let mut source = RtspPacketSource::connect(endpoint)
            .await
            .context("could not establish the RTSP DESCRIBE/SETUP/PLAY session")?;
        if source.profile().video.codec != Some(RtspCodec::H264) {
            let _ = source.close().await;
            bail!(
                "rtspVideoUnsupported: v0.3 requires an H.264 video track; H.265 decode integration is scheduled for V3-04"
            );
        }
        let decoder = match build_rtsp_audio_decoder(
            &aac,
            source.profile(),
            audio.sample_rate,
            audio.channels,
        ) {
            Ok(decoder) => decoder,
            Err(error) => {
                let _ = source.close().await;
                return Err(error);
            }
        };
        (SingleInput::Packets(Box::new(source)), decoder)
    } else {
        let input_endpoint =
            Endpoint::from_config(&input.uri, &input.srt, input.secret_ref.as_ref())
                .context("could not prepare the SRT input")?;
        let input_transport = SrtTransport::connect(input_endpoint)
            .await
            .context("could not establish the initial SRT input connection")?;
        (
            SingleInput::Transport(Box::new(input_transport)),
            Box::new(
                aac.audio_decoder()
                    .context("could not create the native AAC decoder")?,
            ),
        )
    };

    let output_transport = SrtTransport::connect(output_endpoint)
        .await
        .context("could not establish the initial SRT output connection")?;

    Ok(SinglePipelineBackends {
        input,
        output: Box::new(output_transport),
        video_decoder: Box::new(video_decoder),
        video_encoder: Box::new(video_encoder),
        audio_decoder,
        audio_encoder: Box::new(audio_encoder),
    })
}

fn build_rtsp_audio_decoder(
    aac: &std::sync::Arc<Libxaac>,
    profile: &RtspMediaProfile,
    output_sample_rate: u32,
    output_channels: u8,
) -> Result<Box<dyn AudioDecoder>> {
    match profile.audio.as_ref() {
        Some(track) if track.codec == Some(RtspCodec::AacLc) => {
            if track.clock_rate != 48_000 || track.channels != Some(2) {
                bail!(
                    "rtspAacProfileUnsupported: v0.3 requires RTSP AAC-LC at 48000 Hz stereo; general normalization is scheduled for V3-05"
                );
            }
            Ok(Box::new(
                aac.audio_decoder()
                    .context("could not create the native AAC decoder")?,
            ))
        }
        Some(track)
            if matches!(
                track.codec,
                Some(RtspCodec::G711Alaw | RtspCodec::G711Mulaw)
            ) =>
        {
            Ok(Box::new(
                G711Decoder::new(
                    track.codec.expect("match guarantees a codec"),
                    output_sample_rate,
                    output_channels,
                )
                .context("could not create the G.711 audio bridge")?,
            ))
        }
        Some(track) => bail!(
            "rtspAudioUnsupported: selected audio track {:?} is not AAC-LC, PCMA, or PCMU",
            track.codec
        ),
        None => Ok(Box::new(aac.audio_decoder().context(
            "could not create the native AAC decoder for the silent program path",
        )?)),
    }
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
