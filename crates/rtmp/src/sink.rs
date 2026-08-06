use std::{
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use aimedia_core::{
    Timestamp,
    backend::{
        BackendError, CodecId, MediaPacket, PacketSink, PacketSinkObserver, PacketSinkOutcome,
        PacketSinkRuntimeStats,
    },
    config::{RtmpConfig, RtmpMode},
};
use async_trait::async_trait;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    task::JoinHandle,
    time::timeout,
};
use tokio_rustls::{
    TlsConnector,
    rustls::{ClientConfig, RootCertStore, pki_types::ServerName},
};

use crate::{
    AacPublisher, AvcPublisher, PublishSession, RtmpError, RtmpErrorCode, RtmpErrorStage,
    SessionEvent, SessionState, parse_endpoint,
};

const IO_BUFFER_BYTES: usize = 64 * 1024;
const MAX_PEER_READS_PER_PACKET: usize = 4;

trait RtmpIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T> RtmpIo for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

struct Connection {
    io: Box<dyn RtmpIo>,
    session: PublishSession,
}

impl fmt::Debug for Connection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Connection")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

/// RTMP/RTMPS publisher that consumes encoded H.264 Annex-B and AAC ADTS packets.
///
/// The sink owns no media backlog. A failed write drops the current access unit, reconnects in
/// the background, and gates all output until a fresh H.264 IDR with SPS/PPS is available.
pub struct RtmpPacketSink {
    uri: String,
    config: RtmpConfig,
    connection: Option<Connection>,
    reconnect_task: Option<JoinHandle<Result<Connection, RtmpError>>>,
    next_retry: Instant,
    backoff: Duration,
    waiting_for_idr: bool,
    avc: AvcPublisher,
    aac: AacPublisher,
    stats: Arc<RtmpSinkStats>,
    closed: bool,
}

impl RtmpPacketSink {
    pub async fn connect(uri: &str, config: &RtmpConfig) -> Result<Self, RtmpError> {
        if config.mode != RtmpMode::Publish {
            return Err(RtmpError::new(
                RtmpErrorCode::InvalidMode,
                RtmpErrorStage::Configuration,
                false,
                "RTMP packet sink requires publish mode",
            ));
        }
        let endpoint = parse_endpoint(uri, true)?;
        let connection = connect_once(uri.to_owned(), config.clone()).await?;
        let stats = Arc::new(RtmpSinkStats::new(if endpoint.tls { "tls" } else { "tcp" }));
        stats.connected.store(true, Ordering::Relaxed);
        Ok(Self {
            uri: uri.to_owned(),
            config: config.clone(),
            connection: Some(connection),
            reconnect_task: None,
            next_retry: Instant::now(),
            backoff: Duration::from_millis(config.reconnect.initial_backoff_ms),
            waiting_for_idr: true,
            avc: AvcPublisher::default(),
            aac: AacPublisher::default(),
            stats,
            closed: false,
        })
    }

    async fn ensure_connected(&mut self) -> Result<(), BackendError> {
        if self.closed {
            return Err(BackendError::Processing(
                "RTMP publisher is already closed".to_owned(),
            ));
        }
        if self.connection.is_some() {
            return Ok(());
        }
        if !self.config.reconnect.enabled {
            return Err(BackendError::Processing(
                "RTMP publisher disconnected and reconnect is disabled".to_owned(),
            ));
        }

        if self
            .reconnect_task
            .as_ref()
            .is_some_and(JoinHandle::is_finished)
        {
            let result = self
                .reconnect_task
                .take()
                .expect("finished reconnect task must exist")
                .await
                .map_err(|error| {
                    BackendError::Io(format!("RTMP reconnect task failed to join: {error}"))
                })?;
            match result {
                Ok(connection) => {
                    self.connection = Some(connection);
                    self.stats.connected.store(true, Ordering::Relaxed);
                    self.stats.reconnects.fetch_add(1, Ordering::Relaxed);
                    self.backoff = Duration::from_millis(self.config.reconnect.initial_backoff_ms);
                    self.reset_media_gate();
                    return Ok(());
                }
                Err(error) => {
                    if error.retryable {
                        self.schedule_retry();
                    }
                    return Err(backend_protocol(error));
                }
            }
        }
        if self.reconnect_task.is_some() {
            return Err(reconnect_pending());
        }
        if Instant::now() < self.next_retry {
            return Err(reconnect_pending());
        }

        let uri = self.uri.clone();
        let config = self.config.clone();
        self.reconnect_task = Some(tokio::spawn(connect_once(uri, config)));
        Err(reconnect_pending())
    }

    fn schedule_retry(&mut self) {
        self.next_retry = Instant::now() + self.backoff;
        let doubled = self.backoff.saturating_mul(2);
        self.backoff = doubled.min(Duration::from_millis(self.config.reconnect.max_backoff_ms));
    }

    fn disconnect(&mut self) {
        if let Some(mut connection) = self.connection.take() {
            let _ = connection.session.peer_closed();
        }
        self.stats.connected.store(false, Ordering::Relaxed);
        self.next_retry = Instant::now();
        self.reset_media_gate();
    }

    fn reset_media_gate(&mut self) {
        self.waiting_for_idr = true;
        self.avc.reset();
        self.aac.reset();
    }

    async fn publish(&mut self, packet: MediaPacket) -> Result<PacketSinkOutcome, BackendError> {
        if packet.discontinuity {
            self.reset_media_gate();
        }
        self.ensure_connected().await?;
        if self.waiting_for_idr && (packet.codec != CodecId::H264 || !packet.keyframe) {
            return Ok(PacketSinkOutcome::DroppedAwaitingKeyframe);
        }

        let connection = self
            .connection
            .as_mut()
            .expect("ensure_connected guarantees an active connection");
        service_peer(
            connection,
            Duration::from_millis(self.config.read_timeout_ms),
        )
        .await?;

        match packet.codec {
            CodecId::H264 => {
                let dts = packet.dts.unwrap_or(packet.pts);
                let dts_ms = timestamp_millis(dts)?;
                let pts_ms = timestamp_millis(packet.pts)?;
                let composition_offset_ms = i32::try_from(pts_ms - dts_ms).map_err(|_| {
                    BackendError::Unsupported(
                        "H.264 composition timestamp offset does not fit i32 milliseconds"
                            .to_owned(),
                    )
                })?;
                let frames = self
                    .avc
                    .push_annex_b(
                        wrap_rtmp_millis(dts_ms),
                        composition_offset_ms,
                        &packet.data,
                    )
                    .map_err(media_error)?;
                for frame in frames {
                    connection
                        .session
                        .send_video(frame)
                        .map_err(backend_protocol)?;
                }
                flush_outbound(
                    connection,
                    Duration::from_millis(self.config.read_timeout_ms),
                )
                .await?;
                if packet.keyframe {
                    self.waiting_for_idr = false;
                }
            }
            CodecId::AacLc => {
                let timestamp_ms = timestamp_millis(packet.pts)?;
                let frames = self
                    .aac
                    .push_adts(wrap_rtmp_millis(timestamp_ms), &packet.data)
                    .map_err(media_error)?;
                for frame in frames {
                    connection
                        .session
                        .send_audio(frame)
                        .map_err(backend_protocol)?;
                }
                flush_outbound(
                    connection,
                    Duration::from_millis(self.config.read_timeout_ms),
                )
                .await?;
            }
            codec => {
                return Err(BackendError::Unsupported(format!(
                    "RTMP output does not support encoded codec {codec:?}"
                )));
            }
        }

        self.stats.record_packet();
        Ok(PacketSinkOutcome::Sent)
    }
}

impl fmt::Debug for RtmpPacketSink {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let endpoint = parse_endpoint(&self.uri, true).ok();
        formatter
            .debug_struct("RtmpPacketSink")
            .field("host", &endpoint.as_ref().map(|value| value.host.as_str()))
            .field("port", &endpoint.as_ref().map(|value| value.port))
            .field("tls", &endpoint.as_ref().map(|value| value.tls))
            .field("stream_name", &"<redacted>")
            .field("connected", &self.connection.is_some())
            .field("waiting_for_idr", &self.waiting_for_idr)
            .finish()
    }
}

#[async_trait]
impl PacketSink for RtmpPacketSink {
    async fn send_packet(
        &mut self,
        packet: MediaPacket,
    ) -> Result<PacketSinkOutcome, BackendError> {
        match self.publish(packet).await {
            Err(error @ BackendError::Io(_)) => {
                if self.connection.is_some() {
                    self.disconnect();
                }
                Err(error)
            }
            result => result,
        }
    }

    async fn close(&mut self) -> Result<(), BackendError> {
        self.closed = true;
        if let Some(task) = self.reconnect_task.take() {
            task.abort();
            let _ = task.await;
        }
        if let Some(mut connection) = self.connection.take() {
            let _ = connection.session.peer_closed();
            let _ = timeout(
                Duration::from_millis(self.config.read_timeout_ms),
                connection.io.shutdown(),
            )
            .await;
        }
        self.stats.connected.store(false, Ordering::Relaxed);
        Ok(())
    }

    fn observer(&self) -> Option<Arc<dyn PacketSinkObserver>> {
        Some(Arc::new(RtmpObserver {
            state: Arc::clone(&self.stats),
        }))
    }
}

async fn connect_once(uri: String, config: RtmpConfig) -> Result<Connection, RtmpError> {
    let endpoint = parse_endpoint(&uri, true)?;
    let tcp = timeout(
        Duration::from_millis(config.connect_timeout_ms),
        TcpStream::connect((endpoint.host.as_str(), endpoint.port)),
    )
    .await
    .map_err(|_| {
        RtmpError::new(
            RtmpErrorCode::Timeout,
            RtmpErrorStage::Handshake,
            true,
            "RTMP TCP connection did not complete before the configured deadline",
        )
    })?
    .map_err(|_| {
        RtmpError::new(
            RtmpErrorCode::Io,
            RtmpErrorStage::Handshake,
            true,
            "RTMP TCP connection failed",
        )
    })?;
    tcp.set_nodelay(true).map_err(|_| {
        RtmpError::new(
            RtmpErrorCode::Io,
            RtmpErrorStage::Handshake,
            true,
            "could not configure the RTMP TCP socket",
        )
    })?;

    let handshake_timeout = Duration::from_millis(config.handshake_timeout_ms);
    let host = endpoint.host.clone();
    let tls = endpoint.tls;
    let session = PublishSession::from_config(&uri, &config)?;
    timeout(handshake_timeout, async move {
        let io: Box<dyn RtmpIo> = if tls {
            Box::new(connect_tls(tcp, host).await?)
        } else {
            Box::new(tcp)
        };
        complete_handshake(io, session).await
    })
    .await
    .map_err(|_| {
        RtmpError::new(
            RtmpErrorCode::Timeout,
            RtmpErrorStage::Handshake,
            true,
            "RTMP TLS/protocol handshake did not complete before the configured deadline",
        )
    })?
}

async fn connect_tls(
    tcp: TcpStream,
    host: String,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>, RtmpError> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let client = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server_name = ServerName::try_from(host).map_err(|_| {
        RtmpError::new(
            RtmpErrorCode::InvalidEndpoint,
            RtmpErrorStage::Configuration,
            false,
            "RTMPS endpoint host is not a valid TLS server name",
        )
    })?;
    TlsConnector::from(Arc::new(client))
        .connect(server_name, tcp)
        .await
        .map_err(|_| {
            RtmpError::new(
                RtmpErrorCode::Io,
                RtmpErrorStage::Handshake,
                true,
                "RTMPS certificate validation or TLS handshake failed",
            )
        })
}

async fn complete_handshake(
    mut io: Box<dyn RtmpIo>,
    mut session: PublishSession,
) -> Result<Connection, RtmpError> {
    let mut incoming = [0_u8; IO_BUFFER_BYTES];
    loop {
        flush_session(&mut *io, &mut session).await?;
        if session.state() == SessionState::Publishing {
            return Ok(Connection { io, session });
        }
        let received = io.read(&mut incoming).await.map_err(|_| {
            RtmpError::new(
                RtmpErrorCode::Io,
                RtmpErrorStage::Handshake,
                true,
                "RTMP handshake read failed",
            )
        })?;
        if received == 0 {
            return Err(RtmpError::new(
                RtmpErrorCode::Io,
                RtmpErrorStage::Handshake,
                true,
                "RTMP peer closed during handshake",
            ));
        }
        let events = session.feed(&incoming[..received])?;
        if events
            .iter()
            .any(|event| matches!(event, SessionEvent::PeerDisconnected))
        {
            return Err(RtmpError::new(
                RtmpErrorCode::Io,
                RtmpErrorStage::Handshake,
                true,
                "RTMP peer rejected or closed the publishing session",
            ));
        }
    }
}

async fn service_peer(
    connection: &mut Connection,
    write_timeout: Duration,
) -> Result<(), BackendError> {
    for _ in 0..MAX_PEER_READS_PER_PACKET {
        let mut incoming = [0_u8; IO_BUFFER_BYTES];
        let read = connection.io.read(&mut incoming);
        tokio::pin!(read);
        let received = tokio::select! {
            biased;
            result = &mut read => Some(result),
            () = tokio::task::yield_now() => None,
        };
        let Some(received) = received else {
            break;
        };
        let received =
            received.map_err(|_| BackendError::Io("RTMP peer read failed".to_owned()))?;
        if received == 0 {
            return Err(BackendError::Io(
                "RTMP peer closed the publishing socket".to_owned(),
            ));
        }
        let events = connection
            .session
            .feed(&incoming[..received])
            .map_err(backend_protocol)?;
        if events
            .iter()
            .any(|event| matches!(event, SessionEvent::PeerDisconnected))
        {
            return Err(BackendError::Io(
                "RTMP peer closed the publishing session".to_owned(),
            ));
        }
        flush_outbound(connection, write_timeout).await?;
    }
    Ok(())
}

async fn flush_outbound(
    connection: &mut Connection,
    write_timeout: Duration,
) -> Result<(), BackendError> {
    loop {
        let bytes = connection.session.drain_outbound(IO_BUFFER_BYTES);
        if bytes.is_empty() {
            return Ok(());
        }
        timeout(write_timeout, connection.io.write_all(&bytes))
            .await
            .map_err(|_| BackendError::Io("RTMP socket write timed out".to_owned()))?
            .map_err(|_| BackendError::Io("RTMP socket write failed".to_owned()))?;
    }
}

async fn flush_session(io: &mut dyn RtmpIo, session: &mut PublishSession) -> Result<(), RtmpError> {
    loop {
        let bytes = session.drain_outbound(IO_BUFFER_BYTES);
        if bytes.is_empty() {
            return Ok(());
        }
        io.write_all(&bytes).await.map_err(|_| {
            RtmpError::new(
                RtmpErrorCode::Io,
                RtmpErrorStage::Handshake,
                true,
                "RTMP handshake write failed",
            )
        })?;
    }
}

fn timestamp_millis(timestamp: Timestamp) -> Result<i64, BackendError> {
    let millis = i128::from(timestamp.ticks)
        .checked_mul(1_000)
        .and_then(|value| value.checked_div(i128::from(timestamp.timescale)))
        .ok_or_else(|| BackendError::Unsupported("invalid media timestamp".to_owned()))?;
    i64::try_from(millis).map_err(|_| {
        BackendError::Unsupported("media timestamp exceeds i64 milliseconds".to_owned())
    })
}

fn wrap_rtmp_millis(value: i64) -> u32 {
    value.rem_euclid(1_i64 << 32) as u32
}

fn media_error(error: impl fmt::Display) -> BackendError {
    BackendError::Unsupported(format!("RTMP media conversion failed: {error}"))
}

fn backend_protocol(error: RtmpError) -> BackendError {
    if error.retryable {
        backend_io(error)
    } else {
        BackendError::Processing(format!("{:?}: {}", error.code, error.message()))
    }
}

fn backend_io(error: RtmpError) -> BackendError {
    BackendError::Io(format!("{:?}: {}", error.code, error.message()))
}

fn reconnect_pending() -> BackendError {
    BackendError::Io("RTMP reconnect is pending; live packet was not queued".to_owned())
}

#[derive(Debug)]
struct RtmpSinkStats {
    connected: AtomicBool,
    packets_sent: AtomicU64,
    reconnects: AtomicU64,
    last_send: Mutex<Option<Instant>>,
    transport: &'static str,
}

impl RtmpSinkStats {
    fn new(transport: &'static str) -> Self {
        Self {
            connected: AtomicBool::new(false),
            packets_sent: AtomicU64::new(0),
            reconnects: AtomicU64::new(0),
            last_send: Mutex::new(None),
            transport,
        }
    }

    fn record_packet(&self) {
        self.packets_sent.fetch_add(1, Ordering::Relaxed);
        *self.last_send.lock().expect("RTMP stats lock poisoned") = Some(Instant::now());
    }

    fn snapshot(&self) -> PacketSinkRuntimeStats {
        PacketSinkRuntimeStats {
            protocol: "rtmp".to_owned(),
            connected: self.connected.load(Ordering::Relaxed),
            transport: self.transport.to_owned(),
            packets_sent: self.packets_sent.load(Ordering::Relaxed),
            reconnects: self.reconnects.load(Ordering::Relaxed),
            last_send_age_ms: self
                .last_send
                .lock()
                .expect("RTMP stats lock poisoned")
                .map(|at| at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64),
        }
    }
}

#[derive(Debug)]
struct RtmpObserver {
    state: Arc<RtmpSinkStats>,
}

#[async_trait]
impl PacketSinkObserver for RtmpObserver {
    async fn stats(&self) -> Result<PacketSinkRuntimeStats, BackendError> {
        Ok(self.state.snapshot())
    }
}

#[cfg(test)]
mod tests {
    use aimedia_core::{
        Timestamp,
        backend::{CodecId, MediaPacket, PacketSink, PacketSinkOutcome},
        config::ReconnectConfig,
    };
    use bytes::Bytes;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        time::{Duration, timeout},
    };

    use super::*;
    use crate::{ListenerSession, SessionEvent};

    #[tokio::test]
    async fn tcp_sink_publishes_sequence_headers_before_media() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(capture_media(listener));
        let config = RtmpConfig {
            mode: RtmpMode::Publish,
            stream_name: Some("program".to_owned()),
            stream_name_ref: None,
            connect_timeout_ms: 1_000,
            handshake_timeout_ms: 3_000,
            read_timeout_ms: 1_000,
            max_message_bytes: 1024 * 1024,
            reconnect: ReconnectConfig::default(),
        };
        let mut sink = RtmpPacketSink::connect(&format!("rtmp://{address}/live"), &config)
            .await
            .unwrap();

        let outcome = sink.send_packet(video_packet()).await.unwrap();
        assert_eq!(outcome, PacketSinkOutcome::Sent);
        let outcome = sink.send_packet(audio_packet()).await.unwrap();
        assert_eq!(outcome, PacketSinkOutcome::Sent);
        let events = timeout(Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(events[0], SessionEvent::Video(ref frame) if frame.packet_kind == Some(crate::RawAvcPacketKind::SequenceHeader))
        );
        assert!(
            matches!(events[1], SessionEvent::Video(ref frame) if frame.packet_kind == Some(crate::RawAvcPacketKind::NalUnit))
        );
        assert!(matches!(events[2], SessionEvent::Audio(ref frame) if frame.sequence_header));
        assert!(matches!(events[3], SessionEvent::Audio(ref frame) if !frame.sequence_header));

        let stats = sink.observer().unwrap().stats().await.unwrap();
        assert_eq!(stats.packets_sent, 2);
        assert_eq!(stats.transport, "tcp");
        sink.close().await.unwrap();
    }

    #[tokio::test]
    async fn sink_drops_audio_until_the_first_idr() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut io, _) = listener.accept().await.unwrap();
            let mut session = ListenerSession::new("live", "program", 1024 * 1024).unwrap();
            let mut buffer = [0_u8; IO_BUFFER_BYTES];
            loop {
                flush_listener(&mut io, &mut session).await;
                if session.state() == SessionState::Publishing {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    return;
                }
                let read = io.read(&mut buffer).await.unwrap();
                assert!(read > 0);
                session.feed(&buffer[..read]).unwrap();
            }
        });
        let config = test_config();
        let mut sink = RtmpPacketSink::connect(&format!("rtmp://{address}/live"), &config)
            .await
            .unwrap();
        assert_eq!(
            sink.send_packet(audio_packet()).await.unwrap(),
            PacketSinkOutcome::DroppedAwaitingKeyframe
        );
        sink.close().await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn reconnect_drops_history_and_requires_a_fresh_idr() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (first, _) = accept_publisher(&listener).await;
            drop(first);

            let (mut io, mut session) = accept_publisher(&listener).await;
            let mut video = Vec::new();
            let mut buffer = [0_u8; IO_BUFFER_BYTES];
            while video.len() < 2 {
                flush_listener(&mut io, &mut session).await;
                let read = io.read(&mut buffer).await.unwrap();
                assert!(read > 0);
                for event in session.feed(&buffer[..read]).unwrap() {
                    if matches!(event, SessionEvent::Video(_)) {
                        video.push(event);
                    }
                }
            }
            video
        });
        let mut config = test_config();
        config.reconnect.initial_backoff_ms = 10;
        config.reconnect.max_backoff_ms = 20;
        let mut sink = RtmpPacketSink::connect(&format!("rtmp://{address}/live"), &config)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut disconnected = false;
        for _ in 0..20 {
            match sink.send_packet(video_packet()).await {
                Err(BackendError::Io(_)) => {
                    disconnected = true;
                    break;
                }
                Ok(PacketSinkOutcome::Sent) => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                result => panic!("unexpected first-connection result: {result:?}"),
            }
        }
        assert!(disconnected, "closed publisher should be detected");

        let mut gated_after_reconnect = false;
        for _ in 0..100 {
            match sink.send_packet(audio_packet()).await {
                Ok(PacketSinkOutcome::DroppedAwaitingKeyframe) => {
                    gated_after_reconnect = true;
                    break;
                }
                Err(BackendError::Io(_)) => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                result => panic!("unexpected reconnect result: {result:?}"),
            }
        }
        assert!(
            gated_after_reconnect,
            "reconnected output should wait for IDR"
        );
        assert_eq!(
            sink.send_packet(video_packet()).await.unwrap(),
            PacketSinkOutcome::Sent
        );

        let video = timeout(Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(video[0], SessionEvent::Video(ref frame) if frame.packet_kind == Some(crate::RawAvcPacketKind::SequenceHeader))
        );
        assert!(
            matches!(video[1], SessionEvent::Video(ref frame) if frame.packet_kind == Some(crate::RawAvcPacketKind::NalUnit))
        );
        let stats = sink.observer().unwrap().stats().await.unwrap();
        assert_eq!(stats.reconnects, 1);
        sink.close().await.unwrap();
    }

    async fn capture_media(listener: TcpListener) -> Vec<SessionEvent> {
        let (mut io, _) = listener.accept().await.unwrap();
        let mut session = ListenerSession::new("live", "program", 1024 * 1024).unwrap();
        let mut media = Vec::new();
        let mut buffer = [0_u8; IO_BUFFER_BYTES];
        while media.len() < 4 {
            flush_listener(&mut io, &mut session).await;
            let read = io.read(&mut buffer).await.unwrap();
            assert!(read > 0);
            for event in session.feed(&buffer[..read]).unwrap() {
                if matches!(event, SessionEvent::Video(_) | SessionEvent::Audio(_)) {
                    media.push(event);
                }
            }
        }
        media
    }

    async fn accept_publisher(listener: &TcpListener) -> (TcpStream, ListenerSession) {
        let (mut io, _) = listener.accept().await.unwrap();
        let mut session = ListenerSession::new("live", "program", 1024 * 1024).unwrap();
        let mut buffer = [0_u8; IO_BUFFER_BYTES];
        loop {
            flush_listener(&mut io, &mut session).await;
            if session.state() == SessionState::Publishing {
                return (io, session);
            }
            let read = io.read(&mut buffer).await.unwrap();
            assert!(read > 0);
            session.feed(&buffer[..read]).unwrap();
        }
    }

    async fn flush_listener(io: &mut TcpStream, session: &mut ListenerSession) {
        loop {
            let bytes = session.drain_outbound(IO_BUFFER_BYTES);
            if bytes.is_empty() {
                return;
            }
            io.write_all(&bytes).await.unwrap();
        }
    }

    fn test_config() -> RtmpConfig {
        RtmpConfig {
            mode: RtmpMode::Publish,
            stream_name: Some("program".to_owned()),
            stream_name_ref: None,
            connect_timeout_ms: 1_000,
            handshake_timeout_ms: 3_000,
            read_timeout_ms: 1_000,
            max_message_bytes: 1024 * 1024,
            reconnect: ReconnectConfig::default(),
        }
    }

    fn video_packet() -> MediaPacket {
        let annex_b = [
            &[0, 0, 0, 1, 0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9][..],
            &[0, 0, 0, 1, 0x68, 0xee, 0x3c, 0x80][..],
            &[0, 0, 0, 1, 0x65, 0x88, 0x84][..],
        ]
        .concat();
        MediaPacket {
            stream_id: 0,
            codec: CodecId::H264,
            pts: Timestamp::new(9_000, 90_000),
            dts: Some(Timestamp::new(9_000, 90_000)),
            duration: None,
            keyframe: true,
            discontinuity: false,
            data: Bytes::from(annex_b),
        }
    }

    fn audio_packet() -> MediaPacket {
        MediaPacket {
            stream_id: 1,
            codec: CodecId::AacLc,
            pts: Timestamp::new(4_800, 48_000),
            dts: None,
            duration: None,
            keyframe: false,
            discontinuity: false,
            data: Bytes::from_static(&[
                0xff, 0xf1, 0x4c, 0x80, 0x01, 0x7f, 0xfc, 0x11, 0x22, 0x33, 0x44,
            ]),
        }
    }
}
