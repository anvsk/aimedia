//! Bounded RTMP protocol boundary for aimedia.
//!
//! The crate deliberately performs no socket or TLS I/O. It wraps the selected RTMP state
//! machine behind aimedia-owned types and rejects oversized messages and chunk-stream sprays
//! before they reach the protocol decoder.

mod ingress;

use std::fmt;

use aimedia_core::config::{RtmpConfig, RtmpMode};
use bytes::Bytes;
use ingress::IngressGuard;
use shiguredo_rtmp::{
    AudioFormat, AvcPacketType, RtmpConnectionEvent, RtmpConnectionState,
    RtmpPublishClientConnection, RtmpServerConnection, RtmpUrl, VideoCodec, VideoFrameType,
};
use thiserror::Error;
use url::Url;

const MIN_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_EVENTS_PER_FEED: usize = 1024;
const MAX_CONTROL_SEND_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtmpErrorCode {
    InvalidEndpoint,
    InvalidMode,
    InvalidSecret,
    InvalidState,
    MalformedData,
    MessageTooLarge,
    ResourceLimit,
    Protocol,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtmpErrorStage {
    Configuration,
    Handshake,
    Command,
    Receive,
    Send,
}

#[derive(Debug, Error)]
#[error("{code:?} during {stage:?}: {message}")]
pub struct RtmpError {
    pub code: RtmpErrorCode,
    pub stage: RtmpErrorStage,
    pub retryable: bool,
    message: String,
}

impl RtmpError {
    fn new(
        code: RtmpErrorCode,
        stage: RtmpErrorStage,
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

    fn protocol(stage: RtmpErrorStage, error: shiguredo_rtmp::Error) -> Self {
        Self::new(
            RtmpErrorCode::Protocol,
            stage,
            false,
            format!("RTMP protocol rejected input ({:?})", error.kind),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Handshaking,
    Connecting,
    Connected,
    StreamCreated,
    PublishPending,
    Publishing,
    Disconnecting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    Publish,
    Play,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawVideoCodec {
    Avc,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawAvcPacketKind {
    SequenceHeader,
    NalUnit,
    EndOfSequence,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawVideoFrame {
    pub timestamp_ms: u32,
    pub composition_offset_ms: i32,
    pub codec: RawVideoCodec,
    pub packet_kind: Option<RawAvcPacketKind>,
    pub keyframe: bool,
    pub payload: Bytes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawAudioFrame {
    pub timestamp_ms: u32,
    pub aac: bool,
    pub sequence_header: bool,
    pub payload: Bytes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    StateChanged(SessionState),
    PublishAccepted,
    RequestRejected { kind: RequestKind },
    Video(RawVideoFrame),
    Audio(RawAudioFrame),
    PeerDisconnected,
    Ignored,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionStats {
    pub bytes_received: u64,
    pub bytes_sent: u64,
    pub messages_received: u64,
    pub chunk_streams: usize,
    pub peak_ingress_buffer_bytes: usize,
    pub pending_outbound_bytes: usize,
}

/// One server-side RTMP publisher session. Socket accept and timeouts stay in the runtime crate.
pub struct ListenerSession {
    connection: RtmpServerConnection,
    guard: IngressGuard,
    expected_app: String,
    expected_stream_name: String,
    bytes_received: u64,
    bytes_sent: u64,
    closed: bool,
}

impl ListenerSession {
    pub fn from_config(uri: &str, config: &RtmpConfig) -> Result<Self, RtmpError> {
        if config.mode != RtmpMode::Listen {
            return Err(RtmpError::new(
                RtmpErrorCode::InvalidMode,
                RtmpErrorStage::Configuration,
                false,
                "RTMP input session requires listen mode",
            ));
        }
        let endpoint = parse_endpoint(uri, false)?;
        let stream_name = config.resolve_stream_name().map_err(|_| {
            RtmpError::new(
                RtmpErrorCode::InvalidSecret,
                RtmpErrorStage::Configuration,
                false,
                "RTMP streamNameRef could not be resolved to a valid value",
            )
        })?;
        Self::new(endpoint.app, stream_name, config.max_message_bytes as usize)
    }

    pub fn new(
        app: impl Into<String>,
        stream_name: impl Into<String>,
        max_message_bytes: usize,
    ) -> Result<Self, RtmpError> {
        validate_limits(max_message_bytes)?;
        let app = app.into();
        let stream_name = stream_name.into();
        if app.is_empty() || app.len() > 512 || app.contains(['\r', '\n', '?', '#']) {
            return Err(invalid_endpoint("RTMP application path is invalid"));
        }
        validate_resolved_stream_name(&stream_name)?;

        Ok(Self {
            connection: RtmpServerConnection::new(),
            guard: IngressGuard::new(max_message_bytes),
            expected_app: app,
            expected_stream_name: stream_name,
            bytes_received: 0,
            bytes_sent: 0,
            closed: false,
        })
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<SessionEvent>, RtmpError> {
        if self.closed {
            return Err(closed_session_error());
        }
        self.guard.inspect(bytes)?;
        self.connection
            .feed_recv_buf(bytes)
            .map_err(|error| RtmpError::protocol(self.current_stage(), error))?;
        self.bytes_received = self.bytes_received.saturating_add(bytes.len() as u64);

        let mut events = Vec::new();
        while let Some(event) = self.connection.next_event() {
            if events.len() >= MAX_EVENTS_PER_FEED {
                return Err(RtmpError::new(
                    RtmpErrorCode::ResourceLimit,
                    RtmpErrorStage::Receive,
                    false,
                    "one RTMP feed produced more than 1024 events",
                ));
            }
            self.handle_event(event, &mut events)?;
        }
        self.ensure_send_bound()?;
        Ok(events)
    }

    fn handle_event(
        &mut self,
        event: RtmpConnectionEvent,
        events: &mut Vec<SessionEvent>,
    ) -> Result<(), RtmpError> {
        match event {
            RtmpConnectionEvent::PublishRequested {
                app, stream_name, ..
            } if app == self.expected_app && stream_name == self.expected_stream_name => {
                self.connection
                    .accept()
                    .map_err(|error| RtmpError::protocol(RtmpErrorStage::Command, error))?;
                events.push(SessionEvent::PublishAccepted);
            }
            RtmpConnectionEvent::PublishRequested { .. } => {
                self.connection
                    .reject("stream unavailable")
                    .map_err(|error| RtmpError::protocol(RtmpErrorStage::Command, error))?;
                events.push(SessionEvent::RequestRejected {
                    kind: RequestKind::Publish,
                });
            }
            RtmpConnectionEvent::PlayRequested { .. } => {
                self.connection
                    .reject("play is not supported")
                    .map_err(|error| RtmpError::protocol(RtmpErrorStage::Command, error))?;
                events.push(SessionEvent::RequestRejected {
                    kind: RequestKind::Play,
                });
            }
            RtmpConnectionEvent::VideoReceived(frame) => {
                events.push(SessionEvent::Video(RawVideoFrame {
                    timestamp_ms: frame.timestamp.as_millis(),
                    composition_offset_ms: frame.composition_timestamp_offset.as_millis(),
                    codec: if frame.codec == VideoCodec::Avc {
                        RawVideoCodec::Avc
                    } else {
                        RawVideoCodec::Unsupported
                    },
                    packet_kind: frame.avc_packet_type.map(map_avc_packet_kind),
                    keyframe: frame.frame_type == VideoFrameType::KeyFrame,
                    payload: Bytes::from(frame.data),
                }));
            }
            RtmpConnectionEvent::AudioReceived(frame) => {
                events.push(SessionEvent::Audio(RawAudioFrame {
                    timestamp_ms: frame.timestamp.as_millis(),
                    aac: frame.format == AudioFormat::Aac,
                    sequence_header: frame.is_aac_sequence_header,
                    payload: Bytes::from(frame.data),
                }));
            }
            RtmpConnectionEvent::StateChanged(state) => {
                if let Some(state) = map_server_state(state) {
                    events.push(SessionEvent::StateChanged(state));
                }
            }
            RtmpConnectionEvent::DisconnectedByPeer { .. } => {
                events.push(SessionEvent::PeerDisconnected);
            }
            RtmpConnectionEvent::CommandIgnored { .. }
            | RtmpConnectionEvent::MessageIgnored { .. }
            | RtmpConnectionEvent::UserControlEventIgnored { .. } => {
                events.push(SessionEvent::Ignored);
            }
        }
        Ok(())
    }

    fn current_stage(&self) -> RtmpErrorStage {
        match self.connection.state() {
            RtmpConnectionState::Handshaking => RtmpErrorStage::Handshake,
            RtmpConnectionState::Connecting
            | RtmpConnectionState::Connected
            | RtmpConnectionState::MediaStreamCreated
            | RtmpConnectionState::PublishPending
            | RtmpConnectionState::PlayPending => RtmpErrorStage::Command,
            _ => RtmpErrorStage::Receive,
        }
    }

    pub fn drain_outbound(&mut self, max_bytes: usize) -> Bytes {
        if self.closed {
            return Bytes::new();
        }
        let count = max_bytes.min(self.connection.send_buf().len());
        let bytes = Bytes::copy_from_slice(&self.connection.send_buf()[..count]);
        self.connection.advance_send_buf(count);
        self.bytes_sent = self.bytes_sent.saturating_add(count as u64);
        bytes
    }

    pub fn stats(&self) -> SessionStats {
        SessionStats {
            bytes_received: self.bytes_received,
            bytes_sent: self.bytes_sent,
            messages_received: self.guard.messages_seen(),
            chunk_streams: self.guard.chunk_streams(),
            peak_ingress_buffer_bytes: self.guard.peak_buffer_bytes(),
            pending_outbound_bytes: self.connection.send_buf().len(),
        }
    }

    pub fn state(&self) -> SessionState {
        if self.closed {
            return SessionState::Disconnecting;
        }
        map_server_state(self.connection.state()).unwrap_or(SessionState::Disconnecting)
    }

    /// Marks the transport as closed and drops protocol bytes that were not written to the peer.
    pub fn peer_closed(&mut self) -> Option<SessionEvent> {
        if self.closed {
            return None;
        }
        let pending = self.connection.send_buf().len();
        self.connection.advance_send_buf(pending);
        self.closed = true;
        Some(SessionEvent::PeerDisconnected)
    }

    fn ensure_send_bound(&self) -> Result<(), RtmpError> {
        if self.connection.send_buf().len() > MAX_CONTROL_SEND_BYTES {
            return Err(RtmpError::new(
                RtmpErrorCode::ResourceLimit,
                RtmpErrorStage::Send,
                false,
                "RTMP control send buffer exceeds 1 MiB",
            ));
        }
        Ok(())
    }
}

impl fmt::Debug for ListenerSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ListenerSession")
            .field("state", &self.state())
            .field("app", &self.expected_app)
            .field("stream_name", &"<redacted>")
            .field("stats", &self.stats())
            .finish()
    }
}

/// Client-side RTMP publish command session. TLS and socket ownership are deliberately separate.
pub struct PublishSession {
    connection: RtmpPublishClientConnection,
    guard: IngressGuard,
    host: String,
    port: u16,
    tls: bool,
    bytes_received: u64,
    bytes_sent: u64,
    closed: bool,
}

impl PublishSession {
    pub fn from_config(uri: &str, config: &RtmpConfig) -> Result<Self, RtmpError> {
        if config.mode != RtmpMode::Publish {
            return Err(RtmpError::new(
                RtmpErrorCode::InvalidMode,
                RtmpErrorStage::Configuration,
                false,
                "RTMP output session requires publish mode",
            ));
        }
        let endpoint = parse_endpoint(uri, true)?;
        let stream_name = config.resolve_stream_name().map_err(|_| {
            RtmpError::new(
                RtmpErrorCode::InvalidSecret,
                RtmpErrorStage::Configuration,
                false,
                "RTMP streamNameRef could not be resolved to a valid value",
            )
        })?;
        Self::new(uri, stream_name, config.max_message_bytes as usize).map(|mut session| {
            session.host = endpoint.host;
            session.port = endpoint.port;
            session.tls = endpoint.tls;
            session
        })
    }

    pub fn new(
        uri: &str,
        stream_name: impl Into<String>,
        max_message_bytes: usize,
    ) -> Result<Self, RtmpError> {
        validate_limits(max_message_bytes)?;
        let stream_name = stream_name.into();
        validate_resolved_stream_name(&stream_name)?;
        let endpoint = parse_endpoint(uri, true)?;
        let url = RtmpUrl::parse_with_stream_name(uri, &stream_name)
            .map_err(|_| invalid_endpoint("RTMP publisher endpoint is invalid"))?;
        let connection = RtmpPublishClientConnection::new(url);
        if connection.send_buf().len() > MAX_CONTROL_SEND_BYTES {
            return Err(RtmpError::new(
                RtmpErrorCode::ResourceLimit,
                RtmpErrorStage::Send,
                false,
                "initial RTMP handshake exceeds the control send bound",
            ));
        }

        Ok(Self {
            connection,
            guard: IngressGuard::new(max_message_bytes),
            host: endpoint.host,
            port: endpoint.port,
            tls: endpoint.tls,
            bytes_received: 0,
            bytes_sent: 0,
            closed: false,
        })
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<SessionEvent>, RtmpError> {
        if self.closed {
            return Err(closed_session_error());
        }
        self.guard.inspect(bytes)?;
        self.connection
            .feed_recv_buf(bytes)
            .map_err(|error| RtmpError::protocol(self.current_stage(), error))?;
        self.bytes_received = self.bytes_received.saturating_add(bytes.len() as u64);

        let mut events = Vec::new();
        while let Some(event) = self.connection.next_event() {
            if events.len() >= MAX_EVENTS_PER_FEED {
                return Err(RtmpError::new(
                    RtmpErrorCode::ResourceLimit,
                    RtmpErrorStage::Receive,
                    false,
                    "one RTMP feed produced more than 1024 events",
                ));
            }
            match event {
                RtmpConnectionEvent::StateChanged(state) => {
                    if let Some(state) = map_publish_state(state) {
                        events.push(SessionEvent::StateChanged(state));
                    }
                }
                RtmpConnectionEvent::DisconnectedByPeer { .. } => {
                    events.push(SessionEvent::PeerDisconnected);
                }
                RtmpConnectionEvent::CommandIgnored { .. }
                | RtmpConnectionEvent::MessageIgnored { .. }
                | RtmpConnectionEvent::UserControlEventIgnored { .. } => {
                    events.push(SessionEvent::Ignored);
                }
                RtmpConnectionEvent::AudioReceived(_)
                | RtmpConnectionEvent::VideoReceived(_)
                | RtmpConnectionEvent::PublishRequested { .. }
                | RtmpConnectionEvent::PlayRequested { .. } => {
                    return Err(RtmpError::new(
                        RtmpErrorCode::InvalidState,
                        RtmpErrorStage::Receive,
                        false,
                        "publisher received an event that belongs to an ingest or play session",
                    ));
                }
            }
        }
        if self.connection.send_buf().len() > MAX_CONTROL_SEND_BYTES {
            return Err(RtmpError::new(
                RtmpErrorCode::ResourceLimit,
                RtmpErrorStage::Send,
                false,
                "RTMP control send buffer exceeds 1 MiB",
            ));
        }
        Ok(events)
    }

    fn current_stage(&self) -> RtmpErrorStage {
        match self.connection.state() {
            RtmpConnectionState::Handshaking => RtmpErrorStage::Handshake,
            RtmpConnectionState::Connecting
            | RtmpConnectionState::Connected
            | RtmpConnectionState::MediaStreamCreated
            | RtmpConnectionState::PublishPending => RtmpErrorStage::Command,
            _ => RtmpErrorStage::Receive,
        }
    }

    pub fn drain_outbound(&mut self, max_bytes: usize) -> Bytes {
        if self.closed {
            return Bytes::new();
        }
        let count = max_bytes.min(self.connection.send_buf().len());
        let bytes = Bytes::copy_from_slice(&self.connection.send_buf()[..count]);
        self.connection.advance_send_buf(count);
        self.bytes_sent = self.bytes_sent.saturating_add(count as u64);
        bytes
    }

    pub fn stats(&self) -> SessionStats {
        SessionStats {
            bytes_received: self.bytes_received,
            bytes_sent: self.bytes_sent,
            messages_received: self.guard.messages_seen(),
            chunk_streams: self.guard.chunk_streams(),
            peak_ingress_buffer_bytes: self.guard.peak_buffer_bytes(),
            pending_outbound_bytes: self.connection.send_buf().len(),
        }
    }

    pub fn state(&self) -> SessionState {
        if self.closed {
            return SessionState::Disconnecting;
        }
        map_publish_state(self.connection.state()).unwrap_or(SessionState::Disconnecting)
    }

    /// Marks the transport as closed and drops protocol bytes that were not written to the peer.
    pub fn peer_closed(&mut self) -> Option<SessionEvent> {
        if self.closed {
            return None;
        }
        let pending = self.connection.send_buf().len();
        self.connection.advance_send_buf(pending);
        self.closed = true;
        Some(SessionEvent::PeerDisconnected)
    }
}

impl fmt::Debug for PublishSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PublishSession")
            .field("state", &self.state())
            .field("host", &self.host)
            .field("port", &self.port)
            .field("tls", &self.tls)
            .field("stream_name", &"<redacted>")
            .field("stats", &self.stats())
            .finish()
    }
}

#[derive(Debug)]
struct Endpoint {
    host: String,
    port: u16,
    app: String,
    tls: bool,
}

fn parse_endpoint(uri: &str, allow_tls: bool) -> Result<Endpoint, RtmpError> {
    let url = Url::parse(uri).map_err(|_| invalid_endpoint("RTMP URI is invalid"))?;
    let tls = url.scheme().eq_ignore_ascii_case("rtmps");
    if !(url.scheme().eq_ignore_ascii_case("rtmp") || (allow_tls && tls)) {
        return Err(invalid_endpoint("RTMP endpoint uses an unsupported scheme"));
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid_endpoint(
            "RTMP endpoint must not contain credentials, query, or fragment",
        ));
    }
    let host = url
        .host_str()
        .filter(|host| !host.is_empty())
        .ok_or_else(|| invalid_endpoint("RTMP endpoint has no host"))?
        .to_owned();
    let port = url
        .port_or_known_default()
        .ok_or_else(|| invalid_endpoint("RTMP endpoint has no valid port"))?;
    let app = url.path().trim_start_matches('/').to_owned();
    if app.is_empty() || app.ends_with('/') || app.contains('%') {
        return Err(invalid_endpoint(
            "RTMP endpoint must contain an unescaped application path without a trailing slash",
        ));
    }
    Ok(Endpoint {
        host,
        port,
        app,
        tls,
    })
}

fn validate_limits(max_message_bytes: usize) -> Result<(), RtmpError> {
    if !(MIN_MESSAGE_BYTES..=MAX_MESSAGE_BYTES).contains(&max_message_bytes) {
        return Err(RtmpError::new(
            RtmpErrorCode::ResourceLimit,
            RtmpErrorStage::Configuration,
            false,
            "maxMessageBytes must be between 64 KiB and 16 MiB",
        ));
    }
    Ok(())
}

fn validate_resolved_stream_name(stream_name: &str) -> Result<(), RtmpError> {
    if stream_name.is_empty() || stream_name.len() > 1024 || stream_name.contains(['\r', '\n']) {
        return Err(RtmpError::new(
            RtmpErrorCode::InvalidSecret,
            RtmpErrorStage::Configuration,
            false,
            "resolved RTMP stream name is empty, too long, or contains a line break",
        ));
    }
    Ok(())
}

fn invalid_endpoint(message: &'static str) -> RtmpError {
    RtmpError::new(
        RtmpErrorCode::InvalidEndpoint,
        RtmpErrorStage::Configuration,
        false,
        message,
    )
}

fn closed_session_error() -> RtmpError {
    RtmpError::new(
        RtmpErrorCode::InvalidState,
        RtmpErrorStage::Receive,
        false,
        "RTMP session is closed and must be replaced",
    )
}

fn map_server_state(state: RtmpConnectionState) -> Option<SessionState> {
    match state {
        RtmpConnectionState::Handshaking => Some(SessionState::Handshaking),
        RtmpConnectionState::Connecting => Some(SessionState::Connecting),
        RtmpConnectionState::Connected => Some(SessionState::Connected),
        RtmpConnectionState::MediaStreamCreated => Some(SessionState::StreamCreated),
        RtmpConnectionState::PublishPending => Some(SessionState::PublishPending),
        RtmpConnectionState::Publishing => Some(SessionState::Publishing),
        RtmpConnectionState::Disconnecting => Some(SessionState::Disconnecting),
        RtmpConnectionState::PlayPending | RtmpConnectionState::Playing => None,
    }
}

fn map_publish_state(state: RtmpConnectionState) -> Option<SessionState> {
    match state {
        RtmpConnectionState::Handshaking => Some(SessionState::Handshaking),
        RtmpConnectionState::Connecting => Some(SessionState::Connecting),
        RtmpConnectionState::Connected => Some(SessionState::Connected),
        RtmpConnectionState::MediaStreamCreated => Some(SessionState::StreamCreated),
        RtmpConnectionState::PublishPending => Some(SessionState::PublishPending),
        RtmpConnectionState::Publishing => Some(SessionState::Publishing),
        RtmpConnectionState::Disconnecting => Some(SessionState::Disconnecting),
        RtmpConnectionState::PlayPending | RtmpConnectionState::Playing => None,
    }
}

fn map_avc_packet_kind(kind: AvcPacketType) -> RawAvcPacketKind {
    match kind {
        AvcPacketType::SequenceHeader => RawAvcPacketKind::SequenceHeader,
        AvcPacketType::NalUnit => RawAvcPacketKind::NalUnit,
        AvcPacketType::EndOfSequence => RawAvcPacketKind::EndOfSequence,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleartext_publish_and_listener_sessions_complete_a_sans_io_loopback() {
        let mut listener = ListenerSession::new("live", "camera", 1024 * 1024).unwrap();
        let mut publisher =
            PublishSession::new("rtmp://127.0.0.1:1935/live", "camera", 1024 * 1024).unwrap();
        let mut accepted = false;

        for _ in 0..32 {
            let client_bytes = publisher.drain_outbound(16 * 1024);
            if !client_bytes.is_empty() {
                accepted |= listener
                    .feed(&client_bytes)
                    .unwrap()
                    .contains(&SessionEvent::PublishAccepted);
            }
            let server_bytes = listener.drain_outbound(16 * 1024);
            if !server_bytes.is_empty() {
                publisher.feed(&server_bytes).unwrap();
            }
            if listener.state() == SessionState::Publishing
                && publisher.state() == SessionState::Publishing
            {
                break;
            }
        }

        assert!(accepted);
        assert_eq!(listener.state(), SessionState::Publishing);
        assert_eq!(publisher.state(), SessionState::Publishing);
        assert!(listener.stats().pending_outbound_bytes <= MAX_CONTROL_SEND_BYTES);
        assert!(publisher.stats().pending_outbound_bytes <= MAX_CONTROL_SEND_BYTES);
    }

    #[test]
    fn debug_output_redacts_the_stream_name() {
        let listener = ListenerSession::new("live", "do-not-print", 1024 * 1024).unwrap();
        let publisher =
            PublishSession::new("rtmp://127.0.0.1:1935/live", "do-not-print", 1024 * 1024).unwrap();

        assert!(!format!("{listener:?}").contains("do-not-print"));
        assert!(!format!("{publisher:?}").contains("do-not-print"));
    }

    #[test]
    fn peer_close_is_idempotent_and_prevents_session_reuse() {
        let mut listener = ListenerSession::new("live", "camera", 1024 * 1024).unwrap();

        assert_eq!(listener.peer_closed(), Some(SessionEvent::PeerDisconnected));
        assert_eq!(listener.peer_closed(), None);
        assert_eq!(listener.state(), SessionState::Disconnecting);
        assert!(listener.drain_outbound(1024).is_empty());
        assert_eq!(
            listener.feed(&[]).unwrap_err().code,
            RtmpErrorCode::InvalidState
        );
    }
}
