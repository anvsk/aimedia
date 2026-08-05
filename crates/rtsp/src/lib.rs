//! RTSP/RTP input boundary for aimedia.
//!
//! The public API intentionally contains only aimedia-owned types. `retina` is an implementation
//! detail, so graph and runtime crates do not inherit its session state or codec item types.

use std::{fmt, num::NonZeroU32, sync::Arc, time::Duration};

use aimedia_core::{
    Timestamp,
    config::{ReconnectConfig, RtspConfig, RtspTransport, SecretError},
};
use bytes::Bytes;
use futures_util::StreamExt;
use retina::{
    client::{
        Credentials, Demuxed, PlayOptions, Session, SessionGroup, SessionOptions, SetupOptions,
        TcpTransportOptions, Transport, UdpTransportOptions,
    },
    codec::{CodecItem, FrameFormat, ParametersRef},
};
use thiserror::Error;
use tokio::time::timeout;
use url::Url;

const MAX_SDP_BYTES: usize = 256 * 1024;
const MAX_TRACKS: usize = 32;
const MAX_TIMESTAMP_JUMP_SECONDS: u32 = 10;

/// Parsed RTSP endpoint with credentials kept out of debug output.
#[derive(Clone)]
pub struct RtspEndpoint {
    url: Url,
    transport: RtspTransport,
    credentials: Option<Credentials>,
    connect_timeout: Duration,
    read_timeout: Duration,
    keepalive_interval: Duration,
    teardown_timeout: Duration,
    reconnect: ReconnectConfig,
}

impl RtspEndpoint {
    pub fn from_config(uri: &str, config: &RtspConfig) -> Result<Self, RtspError> {
        validate_endpoint_settings(config)?;
        let url = Url::parse(uri).map_err(|error| {
            RtspError::new(
                RtspErrorCode::InvalidEndpoint,
                RtspErrorStage::Configuration,
                false,
                format!("invalid RTSP URI: {error}"),
            )
        })?;
        if !url.scheme().eq_ignore_ascii_case("rtsp") {
            return Err(RtspError::new(
                RtspErrorCode::InvalidEndpoint,
                RtspErrorStage::Configuration,
                false,
                "endpoint must use rtsp://",
            ));
        }
        if url.host_str().is_none() {
            return Err(RtspError::new(
                RtspErrorCode::InvalidEndpoint,
                RtspErrorStage::Configuration,
                false,
                "endpoint must include a camera host",
            ));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(RtspError::new(
                RtspErrorCode::InlineCredentials,
                RtspErrorStage::Configuration,
                false,
                "URI userinfo is forbidden; use rtsp.username and rtsp.passwordRef",
            ));
        }
        if url.query_pairs().any(|(key, _)| {
            matches!(
                key.to_ascii_lowercase().as_str(),
                "password" | "passphrase" | "token" | "secret" | "auth"
            )
        }) {
            return Err(RtspError::new(
                RtspErrorCode::InlineCredentials,
                RtspErrorStage::Configuration,
                false,
                "credential-like RTSP query parameters are forbidden; use passwordRef",
            ));
        }

        let password = config.resolve_password().map_err(RtspError::secret)?;
        let credentials = match (config.username.clone(), password) {
            (Some(username), Some(password)) if !username.is_empty() => {
                Some(Credentials { username, password })
            }
            (None, None) => None,
            _ => {
                return Err(RtspError::new(
                    RtspErrorCode::InvalidCredentials,
                    RtspErrorStage::Configuration,
                    false,
                    "username and passwordRef must resolve together",
                ));
            }
        };

        Ok(Self {
            url,
            transport: config.transport,
            credentials,
            connect_timeout: Duration::from_millis(config.connect_timeout_ms),
            read_timeout: Duration::from_millis(config.read_timeout_ms),
            keepalive_interval: Duration::from_millis(config.keepalive_ms),
            teardown_timeout: Duration::from_millis(config.connect_timeout_ms),
            reconnect: config.reconnect.clone(),
        })
    }

    #[must_use]
    pub const fn transport(&self) -> RtspTransport {
        self.transport
    }

    #[must_use]
    pub const fn reconnect(&self) -> &ReconnectConfig {
        &self.reconnect
    }

    /// Configured maintenance interval for the reconnecting runtime in V3-02C.
    /// Retina still negotiates the actual in-session keepalive cadence with the server.
    #[must_use]
    pub const fn keepalive_interval(&self) -> Duration {
        self.keepalive_interval
    }

    #[must_use]
    pub fn retry_delay(&self, attempt: u32) -> Duration {
        let factor = 1_u64 << attempt.min(31);
        let millis = self
            .reconnect
            .initial_backoff_ms
            .saturating_mul(factor)
            .min(self.reconnect.max_backoff_ms);
        Duration::from_millis(millis)
    }
}

fn validate_endpoint_settings(config: &RtspConfig) -> Result<(), RtspError> {
    if !(100..=60_000).contains(&config.connect_timeout_ms)
        || !(500..=120_000).contains(&config.read_timeout_ms)
        || !(1_000..=300_000).contains(&config.keepalive_ms)
    {
        return Err(RtspError::new(
            RtspErrorCode::InvalidEndpoint,
            RtspErrorStage::Configuration,
            false,
            "RTSP timeout settings are outside the supported ranges",
        ));
    }
    if config.reconnect.initial_backoff_ms == 0
        || config.reconnect.initial_backoff_ms > config.reconnect.max_backoff_ms
        || config.reconnect.max_backoff_ms > 60_000
    {
        return Err(RtspError::new(
            RtspErrorCode::InvalidEndpoint,
            RtspErrorStage::Configuration,
            false,
            "RTSP reconnect backoff must satisfy 1 <= initial <= max <= 60000ms",
        ));
    }
    if config.username.as_deref().is_some_and(|username| {
        username.is_empty() || username.len() > 256 || username.contains(['\r', '\n'])
    }) {
        return Err(RtspError::new(
            RtspErrorCode::InvalidCredentials,
            RtspErrorStage::Configuration,
            false,
            "RTSP username is empty, too long, or contains a line break",
        ));
    }
    Ok(())
}

impl fmt::Debug for RtspEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RtspEndpoint")
            .field("host", &self.url.host_str())
            .field("port", &self.url.port_or_known_default())
            .field("transport", &self.transport)
            .field("authenticated", &self.credentials.is_some())
            .field("connect_timeout", &self.connect_timeout)
            .field("read_timeout", &self.read_timeout)
            .field("keepalive_interval", &self.keepalive_interval)
            .field("reconnect_enabled", &self.reconnect.enabled)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtspErrorStage {
    Configuration,
    Describe,
    Setup,
    Play,
    Receive,
    Teardown,
}

impl fmt::Display for RtspErrorStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Configuration => "configuration",
            Self::Describe => "describe",
            Self::Setup => "setup",
            Self::Play => "play",
            Self::Receive => "receive",
            Self::Teardown => "teardown",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtspErrorCode {
    InvalidEndpoint,
    InlineCredentials,
    InvalidCredentials,
    SecretUnavailable,
    AuthenticationFailed,
    DescribeTimeout,
    DescribeFailed,
    SessionTooLarge,
    NoSupportedTracks,
    SetupTimeout,
    SetupFailed,
    PlayTimeout,
    PlayFailed,
    ReadTimeout,
    ReceiveFailed,
    EndOfStream,
    TeardownTimeout,
    TeardownFailed,
}

impl RtspErrorCode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidEndpoint => "invalidEndpoint",
            Self::InlineCredentials => "inlineCredentials",
            Self::InvalidCredentials => "invalidCredentials",
            Self::SecretUnavailable => "secretUnavailable",
            Self::AuthenticationFailed => "authenticationFailed",
            Self::DescribeTimeout => "describeTimeout",
            Self::DescribeFailed => "describeFailed",
            Self::SessionTooLarge => "sessionTooLarge",
            Self::NoSupportedTracks => "noSupportedTracks",
            Self::SetupTimeout => "setupTimeout",
            Self::SetupFailed => "setupFailed",
            Self::PlayTimeout => "playTimeout",
            Self::PlayFailed => "playFailed",
            Self::ReadTimeout => "readTimeout",
            Self::ReceiveFailed => "receiveFailed",
            Self::EndOfStream => "endOfStream",
            Self::TeardownTimeout => "teardownTimeout",
            Self::TeardownFailed => "teardownFailed",
        }
    }
}

impl fmt::Display for RtspErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Error)]
#[error("RTSP {stage} failed ({code}): {message}")]
pub struct RtspError {
    pub code: RtspErrorCode,
    pub stage: RtspErrorStage,
    pub retryable: bool,
    message: String,
}

impl RtspError {
    fn new(
        code: RtspErrorCode,
        stage: RtspErrorStage,
        retryable: bool,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            stage,
            retryable,
            message: message.into(),
        }
    }

    fn secret(error: SecretError) -> Self {
        Self::new(
            RtspErrorCode::SecretUnavailable,
            RtspErrorStage::Configuration,
            false,
            error.to_string(),
        )
    }

    fn from_retina(stage: RtspErrorStage, error: retina::Error) -> Self {
        if matches!(error.status_code(), Some(401 | 403)) {
            return Self::new(
                RtspErrorCode::AuthenticationFailed,
                stage,
                false,
                "camera rejected RTSP credentials",
            );
        }
        let code = match stage {
            RtspErrorStage::Describe => RtspErrorCode::DescribeFailed,
            RtspErrorStage::Setup => RtspErrorCode::SetupFailed,
            RtspErrorStage::Play => RtspErrorCode::PlayFailed,
            RtspErrorStage::Receive => RtspErrorCode::ReceiveFailed,
            RtspErrorStage::Teardown => RtspErrorCode::TeardownFailed,
            RtspErrorStage::Configuration => RtspErrorCode::InvalidEndpoint,
        };
        let retryable = !matches!(error.status_code(), Some(400..=499));
        Self::new(code, stage, retryable, error.to_string())
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtspCodec {
    H264,
    H265,
    AacLc,
    G711Alaw,
    G711Mulaw,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackDescriptor {
    pub stream_id: usize,
    pub kind: TrackKind,
    pub encoding_name: String,
    pub codec: Option<RtspCodec>,
    pub clock_rate: u32,
    pub channels: Option<u16>,
    pub selected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedVideoFrame {
    pub stream_id: usize,
    pub codec: RtspCodec,
    pub pts: Timestamp,
    pub keyframe: bool,
    pub parameters_changed: bool,
    pub packet_loss: u16,
    pub discontinuity: bool,
    pub data: Bytes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedAudioFrame {
    pub stream_id: usize,
    pub codec: RtspCodec,
    pub pts: Timestamp,
    pub frame_samples: u32,
    pub sample_rate: u32,
    pub channels: u16,
    pub packet_loss: u16,
    pub discontinuity: bool,
    pub data: Bytes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtcpPacket {
    pub stream_id: usize,
    pub rtp_timestamp: Option<Timestamp>,
    pub data: Bytes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscontinuityReason {
    Reconnect,
    TimestampReset,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaEvent {
    Video(EncodedVideoFrame),
    Audio(EncodedAudioFrame),
    Rtcp(RtcpPacket),
    Discontinuity(DiscontinuityReason),
}

/// A live, pull-based RTSP source. No internal unbounded media queue is created.
pub struct RtspSession {
    inner: Demuxed,
    tracks: Vec<TrackDescriptor>,
    session_group: Arc<SessionGroup>,
    read_timeout: Duration,
    teardown_timeout: Duration,
}

impl fmt::Debug for RtspSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RtspSession")
            .field("tracks", &self.tracks)
            .field("read_timeout", &self.read_timeout)
            .finish_non_exhaustive()
    }
}

impl RtspSession {
    pub async fn connect(endpoint: RtspEndpoint) -> Result<Self, RtspError> {
        let session_group = Arc::new(SessionGroup::default());
        let options = SessionOptions::default()
            .creds(endpoint.credentials.clone())
            .session_group(Arc::clone(&session_group))
            .user_agent(format!("aimedia/{}", env!("CARGO_PKG_VERSION")));

        let mut session = timeout(
            endpoint.connect_timeout,
            Session::describe(endpoint.url.clone(), options),
        )
        .await
        .map_err(|_| {
            RtspError::new(
                RtspErrorCode::DescribeTimeout,
                RtspErrorStage::Describe,
                true,
                format!(
                    "DESCRIBE exceeded {}ms",
                    endpoint.connect_timeout.as_millis()
                ),
            )
        })?
        .map_err(|error| RtspError::from_retina(RtspErrorStage::Describe, error))?;

        if session.sdp().len() > MAX_SDP_BYTES || session.streams().len() > MAX_TRACKS {
            return Err(RtspError::new(
                RtspErrorCode::SessionTooLarge,
                RtspErrorStage::Describe,
                false,
                format!(
                    "camera described {} SDP bytes and {} tracks; limits are {MAX_SDP_BYTES} and {MAX_TRACKS}",
                    session.sdp().len(),
                    session.streams().len()
                ),
            ));
        }

        let mut tracks: Vec<_> = session
            .streams()
            .iter()
            .enumerate()
            .map(|(stream_id, stream)| describe_track(stream_id, stream))
            .collect();
        select_primary_tracks(&mut tracks);
        let selected: Vec<_> = tracks
            .iter()
            .filter(|track| track.selected)
            .map(|track| track.stream_id)
            .collect();
        if selected.is_empty() {
            let offered = tracks
                .iter()
                .map(|track| format!("{}/{}", kind_name(track.kind), track.encoding_name))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(RtspError::new(
                RtspErrorCode::NoSupportedTracks,
                RtspErrorStage::Describe,
                false,
                format!("no supported H.264/H.265/AAC-LC/G.711 tracks; offered: {offered}"),
            ));
        }

        for stream_id in selected {
            let setup = SetupOptions::default()
                .transport(retina_transport(endpoint.transport))
                .frame_format(FrameFormat::SIMPLE);
            timeout(endpoint.connect_timeout, session.setup(stream_id, setup))
                .await
                .map_err(|_| {
                    RtspError::new(
                        RtspErrorCode::SetupTimeout,
                        RtspErrorStage::Setup,
                        true,
                        format!("SETUP for stream {stream_id} timed out"),
                    )
                })?
                .map_err(|error| RtspError::from_retina(RtspErrorStage::Setup, error))?;
        }

        let playing = timeout(
            endpoint.connect_timeout,
            session.play(
                PlayOptions::default().enforce_timestamps_with_max_jump_secs(
                    NonZeroU32::new(MAX_TIMESTAMP_JUMP_SECONDS).expect("constant is non-zero"),
                ),
            ),
        )
        .await
        .map_err(|_| {
            RtspError::new(
                RtspErrorCode::PlayTimeout,
                RtspErrorStage::Play,
                true,
                "PLAY timed out",
            )
        })?
        .map_err(|error| RtspError::from_retina(RtspErrorStage::Play, error))?;
        let inner = playing
            .demuxed()
            .map_err(|error| RtspError::from_retina(RtspErrorStage::Play, error))?;

        Ok(Self {
            inner,
            tracks,
            session_group,
            read_timeout: endpoint.read_timeout,
            teardown_timeout: endpoint.teardown_timeout,
        })
    }

    #[must_use]
    pub fn tracks(&self) -> &[TrackDescriptor] {
        &self.tracks
    }

    pub async fn next_event(&mut self) -> Result<MediaEvent, RtspError> {
        loop {
            let item = timeout(self.read_timeout, self.inner.next())
                .await
                .map_err(|_| {
                    RtspError::new(
                        RtspErrorCode::ReadTimeout,
                        RtspErrorStage::Receive,
                        true,
                        format!(
                            "no RTSP media arrived for {}ms",
                            self.read_timeout.as_millis()
                        ),
                    )
                })?
                .ok_or_else(|| {
                    RtspError::new(
                        RtspErrorCode::EndOfStream,
                        RtspErrorStage::Receive,
                        true,
                        "camera ended the media stream",
                    )
                })?
                .map_err(|error| RtspError::from_retina(RtspErrorStage::Receive, error))?;

            match item {
                CodecItem::VideoFrame(frame) => {
                    let stream_id = frame.stream_id();
                    let track = selected_track(&self.tracks, stream_id)?;
                    let codec = track.codec.ok_or_else(|| unsupported_track(stream_id))?;
                    let pts = media_timestamp(frame.timestamp());
                    let keyframe = frame.is_random_access_point();
                    let parameters_changed = frame.has_new_parameters();
                    let packet_loss = frame.loss();
                    return Ok(MediaEvent::Video(EncodedVideoFrame {
                        stream_id,
                        codec,
                        pts,
                        keyframe,
                        parameters_changed,
                        packet_loss,
                        discontinuity: packet_loss > 0,
                        data: Bytes::from(frame.into_data()),
                    }));
                }
                CodecItem::AudioFrame(frame) => {
                    let stream_id = frame.stream_id();
                    let track = selected_track(&self.tracks, stream_id)?;
                    let codec = track.codec.ok_or_else(|| unsupported_track(stream_id))?;
                    let packet_loss = frame.loss();
                    return Ok(MediaEvent::Audio(EncodedAudioFrame {
                        stream_id,
                        codec,
                        pts: media_timestamp(frame.timestamp()),
                        frame_samples: frame.frame_length().get(),
                        sample_rate: track.clock_rate,
                        channels: track.channels.unwrap_or(1),
                        packet_loss,
                        discontinuity: packet_loss > 0,
                        data: Bytes::copy_from_slice(frame.data()),
                    }));
                }
                CodecItem::Rtcp(packet) => {
                    return Ok(MediaEvent::Rtcp(RtcpPacket {
                        stream_id: packet.stream_id(),
                        rtp_timestamp: packet.rtp_timestamp().map(media_timestamp),
                        data: Bytes::copy_from_slice(packet.raw()),
                    }));
                }
                CodecItem::MessageFrame(_) => {}
                _ => {}
            }
        }
    }

    pub async fn close(self) -> Result<(), RtspError> {
        let Self {
            inner,
            session_group,
            teardown_timeout,
            ..
        } = self;
        drop(inner);
        timeout(teardown_timeout, session_group.await_teardown())
            .await
            .map_err(|_| {
                RtspError::new(
                    RtspErrorCode::TeardownTimeout,
                    RtspErrorStage::Teardown,
                    true,
                    "RTSP TEARDOWN timed out",
                )
            })?
            .map_err(|error| RtspError::from_retina(RtspErrorStage::Teardown, error))
    }
}

fn describe_track(stream_id: usize, stream: &retina::client::Stream) -> TrackDescriptor {
    let kind = match stream.media() {
        "video" => TrackKind::Video,
        "audio" => TrackKind::Audio,
        _ => TrackKind::Other,
    };
    TrackDescriptor {
        stream_id,
        kind,
        encoding_name: stream.encoding_name().to_owned(),
        codec: classify_codec(stream),
        clock_rate: stream.clock_rate_hz(),
        channels: stream.channels().map(|channels| channels.get()),
        selected: false,
    }
}

fn classify_codec(stream: &retina::client::Stream) -> Option<RtspCodec> {
    match (stream.media(), stream.encoding_name()) {
        ("video", "h264") if stream.clock_rate_hz() == 90_000 => Some(RtspCodec::H264),
        ("video", "h265") if stream.clock_rate_hz() == 90_000 => Some(RtspCodec::H265),
        ("audio", "mpeg4-generic")
            if matches!(stream.clock_rate_hz(), 44_100 | 48_000)
                && matches!(stream.channels().map(|value| value.get()), Some(1 | 2))
                && matches!(
                    stream.parameters(),
                    Some(ParametersRef::Audio(parameters))
                        if parameters.rfc6381_codec() == Some("mp4a.40.2")
                ) =>
        {
            Some(RtspCodec::AacLc)
        }
        ("audio", "pcma")
            if stream.clock_rate_hz() == 8_000
                && stream.channels().is_none_or(|value| value.get() == 1) =>
        {
            Some(RtspCodec::G711Alaw)
        }
        ("audio", "pcmu")
            if stream.clock_rate_hz() == 8_000
                && stream.channels().is_none_or(|value| value.get() == 1) =>
        {
            Some(RtspCodec::G711Mulaw)
        }
        _ => None,
    }
}

fn select_primary_tracks(tracks: &mut [TrackDescriptor]) {
    let mut selected_video = false;
    let mut selected_audio = false;
    for track in tracks {
        track.selected = match track.kind {
            TrackKind::Video if track.codec.is_some() && !selected_video => {
                selected_video = true;
                true
            }
            TrackKind::Audio if track.codec.is_some() && !selected_audio => {
                selected_audio = true;
                true
            }
            TrackKind::Video | TrackKind::Audio | TrackKind::Other => false,
        };
    }
}

fn selected_track(
    tracks: &[TrackDescriptor],
    stream_id: usize,
) -> Result<&TrackDescriptor, RtspError> {
    tracks
        .get(stream_id)
        .filter(|track| track.selected)
        .ok_or_else(|| unsupported_track(stream_id))
}

fn unsupported_track(stream_id: usize) -> RtspError {
    RtspError::new(
        RtspErrorCode::ReceiveFailed,
        RtspErrorStage::Receive,
        false,
        format!("received media for unselected stream {stream_id}"),
    )
}

fn media_timestamp(timestamp: retina::Timestamp) -> Timestamp {
    Timestamp::new(timestamp.timestamp(), timestamp.clock_rate().get())
}

fn retina_transport(transport: RtspTransport) -> Transport {
    match transport {
        RtspTransport::Tcp => Transport::Tcp(TcpTransportOptions::default()),
        RtspTransport::Udp => Transport::Udp(UdpTransportOptions::default()),
    }
}

const fn kind_name(kind: TrackKind) -> &'static str {
    match kind {
        TrackKind::Video => "video",
        TrackKind::Audio => "audio",
        TrackKind::Other => "other",
    }
}

#[cfg(test)]
mod tests {
    use std::{io, path::PathBuf, time::Duration};

    use aimedia_core::config::{ReconnectConfig, RtspConfig, RtspTransport, SecretRef};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };

    use super::{
        MediaEvent, RtspCodec, RtspEndpoint, RtspErrorCode, RtspErrorStage, RtspSession,
        TrackDescriptor, TrackKind, select_primary_tracks,
    };

    fn config() -> RtspConfig {
        RtspConfig {
            transport: RtspTransport::Tcp,
            username: None,
            password_ref: None,
            connect_timeout_ms: 3_000,
            read_timeout_ms: 5_000,
            keepalive_ms: 15_000,
            reconnect: ReconnectConfig::default(),
        }
    }

    #[test]
    fn endpoint_debug_and_errors_do_not_expose_credentials() {
        let error = RtspEndpoint::from_config("rtsp://admin:secret@192.0.2.10/live", &config())
            .expect_err("inline credentials must be rejected");
        assert_eq!(error.code, RtspErrorCode::InlineCredentials);
        assert_eq!(error.stage, RtspErrorStage::Configuration);
        assert!(!error.to_string().contains("secret@"));

        let mut authenticated = config();
        authenticated.username = Some("admin".to_owned());
        authenticated.password_ref = Some(SecretRef {
            env: None,
            file: Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")),
        });
        let endpoint =
            RtspEndpoint::from_config("rtsp://192.0.2.10/live?profile=main", &authenticated)
                .expect("referenced credentials should resolve");
        let debug = format!("{endpoint:?}");
        assert!(!debug.contains("aimedia-rtsp"));
        assert!(!debug.contains("/live"));
        assert_eq!(endpoint.keepalive_interval(), Duration::from_secs(15));
        assert_eq!(endpoint.retry_delay(0), Duration::from_millis(250));
        assert_eq!(endpoint.retry_delay(99), Duration::from_millis(5_000));

        let mut invalid_timeout = config();
        invalid_timeout.connect_timeout_ms = 0;
        let error = RtspEndpoint::from_config("rtsp://192.0.2.10/live", &invalid_timeout)
            .expect_err("adapter boundary should revalidate timeout ranges");
        assert_eq!(error.code, RtspErrorCode::InvalidEndpoint);
    }

    #[test]
    fn only_the_first_supported_video_and_audio_tracks_are_selected() {
        let mut tracks = vec![
            TrackDescriptor {
                stream_id: 0,
                kind: TrackKind::Video,
                encoding_name: "jpeg".to_owned(),
                codec: None,
                clock_rate: 90_000,
                channels: None,
                selected: false,
            },
            TrackDescriptor {
                stream_id: 1,
                kind: TrackKind::Video,
                encoding_name: "h264".to_owned(),
                codec: Some(super::RtspCodec::H264),
                clock_rate: 90_000,
                channels: None,
                selected: false,
            },
            TrackDescriptor {
                stream_id: 2,
                kind: TrackKind::Audio,
                encoding_name: "pcma".to_owned(),
                codec: Some(super::RtspCodec::G711Alaw),
                clock_rate: 8_000,
                channels: Some(1),
                selected: false,
            },
            TrackDescriptor {
                stream_id: 3,
                kind: TrackKind::Audio,
                encoding_name: "pcmu".to_owned(),
                codec: Some(super::RtspCodec::G711Mulaw),
                clock_rate: 8_000,
                channels: Some(1),
                selected: false,
            },
        ];
        select_primary_tracks(&mut tracks);
        assert_eq!(
            tracks
                .iter()
                .filter(|track| track.selected)
                .map(|track| track.stream_id)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[tokio::test]
    async fn tcp_session_describes_sets_up_plays_and_tears_down() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock camera should bind");
        let address = listener.local_addr().expect("mock camera has an address");
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await?;
            serve_mock_camera(socket, address.port()).await
        });

        let endpoint = RtspEndpoint::from_config(&format!("rtsp://{address}/live"), &config())
            .expect("loopback endpoint should be valid");
        let mut session = RtspSession::connect(endpoint)
            .await
            .expect("mock RTSP handshake should complete");
        assert_eq!(
            session
                .tracks()
                .iter()
                .filter(|track| track.selected)
                .map(|track| track.codec)
                .collect::<Vec<_>>(),
            vec![Some(RtspCodec::H264), Some(RtspCodec::AacLc)]
        );

        let event = session
            .next_event()
            .await
            .expect("mock camera should emit one H.264 access unit");
        let MediaEvent::Video(frame) = event else {
            panic!("expected video event");
        };
        assert_eq!(frame.codec, RtspCodec::H264);
        assert!(frame.keyframe);
        assert!(frame.data.starts_with(&[0, 0, 0, 1]));

        session.close().await.expect("TEARDOWN should complete");
        server
            .await
            .expect("mock camera task should not panic")
            .expect("mock camera should complete without I/O errors");
    }

    async fn serve_mock_camera(mut socket: TcpStream, port: u16) -> io::Result<()> {
        let mut pending = Vec::new();
        let mut setup_count = 0_u8;
        loop {
            let request = read_request(&mut socket, &mut pending).await?;
            let request = String::from_utf8_lossy(&request);
            let method = request.split_whitespace().next().unwrap_or_default();
            let cseq = request
                .lines()
                .find_map(|line| {
                    line.split_once(':').and_then(|(name, value)| {
                        name.eq_ignore_ascii_case("cseq")
                            .then(|| value.trim().to_owned())
                    })
                })
                .unwrap_or_else(|| "1".to_owned());
            match method {
                "OPTIONS" => {
                    write_response(
                        &mut socket,
                        &cseq,
                        "Public: OPTIONS, DESCRIBE, SETUP, PLAY, GET_PARAMETER, TEARDOWN\r\n",
                        b"",
                    )
                    .await?;
                }
                "DESCRIBE" => {
                    let sdp = include_bytes!("../../../examples/fixtures/rtsp/h264-aac.sdp");
                    write_response(
                        &mut socket,
                        &cseq,
                        &format!(
                            "Content-Base: rtsp://127.0.0.1:{port}/live/\r\nContent-Type: application/sdp\r\n"
                        ),
                        sdp,
                    )
                    .await?;
                }
                "SETUP" => {
                    let first_channel = setup_count.saturating_mul(2);
                    setup_count = setup_count.saturating_add(1);
                    write_response(
                        &mut socket,
                        &cseq,
                        &format!(
                            "Session: aimedia-test;timeout=60\r\nTransport: RTP/AVP/TCP;unicast;interleaved={first_channel}-{}\r\n",
                            first_channel + 1
                        ),
                        b"",
                    )
                    .await?;
                }
                "PLAY" => {
                    write_response(
                        &mut socket,
                        &cseq,
                        &format!(
                            "Session: aimedia-test\r\nRange: npt=0.000-\r\nRTP-Info: url=rtsp://127.0.0.1:{port}/live/trackID=1;seq=1;rtptime=0,url=rtsp://127.0.0.1:{port}/live/trackID=2;seq=1;rtptime=0\r\n"
                        ),
                        b"",
                    )
                    .await?;
                    write_h264_idr(&mut socket).await?;
                }
                "GET_PARAMETER" => {
                    write_response(&mut socket, &cseq, "Session: aimedia-test\r\n", b"").await?;
                }
                "TEARDOWN" => {
                    write_response(&mut socket, &cseq, "Session: aimedia-test\r\n", b"").await?;
                    return Ok(());
                }
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unexpected RTSP method {other}"),
                    ));
                }
            }
        }
    }

    async fn read_request(socket: &mut TcpStream, pending: &mut Vec<u8>) -> io::Result<Vec<u8>> {
        loop {
            if let Some(end) = pending.windows(4).position(|window| window == b"\r\n\r\n") {
                let request = pending.drain(..end + 4).collect();
                return Ok(request);
            }
            if pending.len() >= 64 * 1024 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "mock RTSP request exceeded 64 KiB",
                ));
            }
            let mut chunk = [0_u8; 4096];
            let read = socket.read(&mut chunk).await?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "RTSP peer closed before the next request",
                ));
            }
            pending.extend_from_slice(&chunk[..read]);
        }
    }

    async fn write_response(
        socket: &mut TcpStream,
        cseq: &str,
        headers: &str,
        body: &[u8],
    ) -> io::Result<()> {
        let head = format!(
            "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\n{headers}Content-Length: {}\r\n\r\n",
            body.len()
        );
        socket.write_all(head.as_bytes()).await?;
        socket.write_all(body).await
    }

    async fn write_h264_idr(socket: &mut TcpStream) -> io::Result<()> {
        let rtp = [
            0x80, 0xe0, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04, 0x65, 0x88,
            0x84, 0x21,
        ];
        let mut interleaved = vec![b'$', 0, 0, rtp.len() as u8];
        interleaved.extend_from_slice(&rtp);
        socket.write_all(&interleaved).await
    }
}
