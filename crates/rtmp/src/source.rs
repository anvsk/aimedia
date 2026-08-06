use std::{
    collections::VecDeque,
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
        BackendError, CodecId, MediaPacket, PacketSource, PacketSourceObserver,
        PacketSourceRuntimeStats,
    },
    config::{RtmpConfig, RtmpMode},
};
use async_trait::async_trait;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

use crate::{
    AacIngest, AvcIngest, ListenerSession, RequestKind, RtmpError, RtmpErrorCode, RtmpErrorStage,
    SessionEvent, SessionState, parse_endpoint, validate_limits, validate_resolved_stream_name,
};

const READ_BUFFER_BYTES: usize = 64 * 1024;
const MAX_PENDING_PACKETS: usize = 256;

struct Peer {
    stream: TcpStream,
    session: ListenerSession,
    avc: AvcIngest,
    aac: AacIngest,
    accepted_at: Instant,
    timestamps: [TimestampUnwrapper; 2],
}

#[derive(Debug, Default)]
struct TimestampUnwrapper {
    epoch_ms: u64,
    last_ms: Option<u32>,
}

impl TimestampUnwrapper {
    fn unwrap(&mut self, timestamp_ms: u32) -> i64 {
        if self
            .last_ms
            .is_some_and(|last| last > timestamp_ms && last - timestamp_ms > i32::MAX as u32)
        {
            self.epoch_ms = self.epoch_ms.saturating_add(1_u64 << 32);
        }
        self.last_ms = Some(timestamp_ms);
        i64::try_from(self.epoch_ms.saturating_add(u64::from(timestamp_ms))).unwrap_or(i64::MAX)
    }
}

impl fmt::Debug for Peer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Peer")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

/// A single-publisher RTMP listener that enters the runtime as H.264 Annex-B and AAC ADTS.
pub struct RtmpPacketSource {
    listener: TcpListener,
    app: String,
    stream_name: String,
    max_message_bytes: usize,
    handshake_timeout: Duration,
    read_timeout: Duration,
    reconnect_enabled: bool,
    peer: Option<Peer>,
    pending: VecDeque<MediaPacket>,
    pending_discontinuity: [bool; 2],
    had_peer: bool,
    closed: bool,
    stats: Arc<RtmpSourceStats>,
}

impl RtmpPacketSource {
    pub async fn bind(uri: &str, config: &RtmpConfig) -> Result<Self, RtmpError> {
        if config.mode != RtmpMode::Listen {
            return Err(RtmpError::new(
                RtmpErrorCode::InvalidMode,
                RtmpErrorStage::Configuration,
                false,
                "RTMP packet source requires listen mode",
            ));
        }
        validate_limits(config.max_message_bytes as usize)?;
        let endpoint = parse_endpoint(uri, false)?;
        let stream_name = config.resolve_stream_name().map_err(|_| {
            RtmpError::new(
                RtmpErrorCode::InvalidSecret,
                RtmpErrorStage::Configuration,
                false,
                "RTMP streamNameRef could not be resolved to a valid value",
            )
        })?;
        validate_resolved_stream_name(&stream_name)?;
        let listener = TcpListener::bind((endpoint.host.as_str(), endpoint.port))
            .await
            .map_err(|_| {
                RtmpError::new(
                    RtmpErrorCode::Io,
                    RtmpErrorStage::Configuration,
                    false,
                    "could not bind the RTMP listener endpoint",
                )
            })?;

        Ok(Self {
            listener,
            app: endpoint.app,
            stream_name,
            max_message_bytes: config.max_message_bytes as usize,
            handshake_timeout: Duration::from_millis(config.handshake_timeout_ms),
            read_timeout: Duration::from_millis(config.read_timeout_ms),
            reconnect_enabled: config.reconnect.enabled,
            peer: None,
            pending: VecDeque::new(),
            pending_discontinuity: [false; 2],
            had_peer: false,
            closed: false,
            stats: Arc::new(RtmpSourceStats::default()),
        })
    }

    pub fn local_addr(&self) -> Result<std::net::SocketAddr, RtmpError> {
        self.listener.local_addr().map_err(|_| {
            RtmpError::new(
                RtmpErrorCode::Io,
                RtmpErrorStage::Configuration,
                false,
                "could not read the RTMP listener address",
            )
        })
    }

    async fn accept_peer(&mut self) -> Result<(), RtmpError> {
        let (stream, _) = self.listener.accept().await.map_err(|_| {
            RtmpError::new(
                RtmpErrorCode::Io,
                RtmpErrorStage::Receive,
                true,
                "RTMP listener accept failed",
            )
        })?;
        stream.set_nodelay(true).map_err(|_| {
            RtmpError::new(
                RtmpErrorCode::Io,
                RtmpErrorStage::Receive,
                true,
                "could not configure the accepted RTMP socket",
            )
        })?;
        let session = ListenerSession::new(
            self.app.clone(),
            self.stream_name.clone(),
            self.max_message_bytes,
        )?;
        if self.had_peer {
            self.pending_discontinuity = [true; 2];
            self.stats.reconnects.fetch_add(1, Ordering::Relaxed);
        }
        self.had_peer = true;
        self.stats.connected.store(false, Ordering::Relaxed);
        self.peer = Some(Peer {
            stream,
            session,
            avc: AvcIngest::default(),
            aac: AacIngest::default(),
            accepted_at: Instant::now(),
            timestamps: std::array::from_fn(|_| TimestampUnwrapper::default()),
        });
        Ok(())
    }

    async fn pump_peer(&mut self) -> Result<(), RtmpError> {
        let mut peer = self.peer.take().ok_or_else(|| {
            RtmpError::new(
                RtmpErrorCode::InvalidState,
                RtmpErrorStage::Receive,
                false,
                "RTMP peer is not available",
            )
        })?;

        let result = async {
            flush_outbound(&mut peer, self.handshake_timeout, self.read_timeout).await?;
            let wait = peer_io_timeout(&peer, self.handshake_timeout, self.read_timeout)?;
            let mut buffer = [0_u8; READ_BUFFER_BYTES];
            let received = timeout(wait, peer.stream.read(&mut buffer))
                .await
                .map_err(|_| {
                    RtmpError::new(
                        RtmpErrorCode::Timeout,
                        RtmpErrorStage::Receive,
                        true,
                        "RTMP peer did not send data before the configured deadline",
                    )
                })?
                .map_err(|_| {
                    RtmpError::new(
                        RtmpErrorCode::Io,
                        RtmpErrorStage::Receive,
                        true,
                        "RTMP socket read failed",
                    )
                })?;
            if received == 0 {
                return Err(RtmpError::new(
                    RtmpErrorCode::Io,
                    RtmpErrorStage::Receive,
                    true,
                    "RTMP publisher closed the connection",
                ));
            }

            let events = peer.session.feed(&buffer[..received])?;
            let mut rejected = None;
            for event in events {
                match event {
                    SessionEvent::PublishAccepted => {
                        self.stats.connected.store(true, Ordering::Relaxed);
                    }
                    SessionEvent::RequestRejected { kind } => rejected = Some(kind),
                    SessionEvent::Video(frame) => {
                        if let Some(access_unit) = peer.avc.push(&frame).map_err(media_error)? {
                            let dts_ms = peer.timestamps[0].unwrap(access_unit.dts_ms);
                            let composition_offset_ms =
                                access_unit.pts_ms - i64::from(access_unit.dts_ms);
                            let discontinuity = std::mem::take(&mut self.pending_discontinuity[0]);
                            self.push_packet(MediaPacket {
                                stream_id: 0,
                                codec: CodecId::H264,
                                pts: Timestamp::new(
                                    dts_ms.saturating_add(composition_offset_ms),
                                    1_000,
                                ),
                                dts: Some(Timestamp::new(dts_ms, 1_000)),
                                duration: None,
                                keyframe: access_unit.keyframe,
                                discontinuity,
                                data: access_unit.annex_b,
                            })?;
                        }
                    }
                    SessionEvent::Audio(frame) => {
                        if let Some(access_unit) = peer.aac.push(&frame).map_err(media_error)? {
                            let timestamp_ms = peer.timestamps[1].unwrap(access_unit.timestamp_ms);
                            let discontinuity = std::mem::take(&mut self.pending_discontinuity[1]);
                            self.push_packet(MediaPacket {
                                stream_id: 1,
                                codec: CodecId::AacLc,
                                pts: Timestamp::new(timestamp_ms, 1_000),
                                dts: Some(Timestamp::new(timestamp_ms, 1_000)),
                                duration: Some(Timestamp::new(1_024, 48_000)),
                                keyframe: true,
                                discontinuity,
                                data: access_unit.adts,
                            })?;
                        }
                    }
                    SessionEvent::PeerDisconnected
                    | SessionEvent::StateChanged(SessionState::Disconnecting) => {
                        return Err(RtmpError::new(
                            RtmpErrorCode::Io,
                            RtmpErrorStage::Receive,
                            true,
                            "RTMP protocol entered the disconnecting state",
                        ));
                    }
                    SessionEvent::StateChanged(_) | SessionEvent::Ignored => {}
                }
            }
            flush_outbound(&mut peer, self.handshake_timeout, self.read_timeout).await?;
            if let Some(kind) = rejected {
                return Err(RtmpError::new(
                    RtmpErrorCode::Protocol,
                    RtmpErrorStage::Command,
                    true,
                    match kind {
                        RequestKind::Publish => "RTMP publisher requested the wrong app or stream",
                        RequestKind::Play => "RTMP play requests are not supported",
                    },
                ));
            }
            Ok(())
        }
        .await;

        if result.is_ok() {
            self.peer = Some(peer);
        } else {
            self.stats.connected.store(false, Ordering::Relaxed);
            let _ = peer.session.peer_closed();
            let _ = peer.stream.shutdown().await;
        }
        result
    }

    fn push_packet(&mut self, packet: MediaPacket) -> Result<(), RtmpError> {
        if self.pending.len() >= MAX_PENDING_PACKETS {
            return Err(RtmpError::new(
                RtmpErrorCode::ResourceLimit,
                RtmpErrorStage::Receive,
                true,
                "one RTMP socket read produced more than 256 media packets",
            ));
        }
        self.pending.push_back(packet);
        Ok(())
    }

    fn pop_packet(&mut self) -> Option<MediaPacket> {
        let packet = self.pending.pop_front()?;
        self.stats.record_packet();
        Some(packet)
    }
}

impl fmt::Debug for RtmpPacketSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RtmpPacketSource")
            .field("local_addr", &self.listener.local_addr().ok())
            .field("app", &self.app)
            .field("stream_name", &"<redacted>")
            .field("connected", &self.stats.connected.load(Ordering::Relaxed))
            .field("pending_packets", &self.pending.len())
            .finish()
    }
}

#[async_trait]
impl PacketSource for RtmpPacketSource {
    async fn receive_packet(&mut self) -> Result<MediaPacket, BackendError> {
        loop {
            if let Some(packet) = self.pop_packet() {
                return Ok(packet);
            }
            if self.closed {
                return Err(BackendError::EndOfStream);
            }
            if self.peer.is_none() {
                if self.had_peer && !self.reconnect_enabled {
                    return Err(BackendError::EndOfStream);
                }
                self.accept_peer().await.map_err(backend_error)?;
            }
            if let Err(error) = self.pump_peer().await {
                if self.reconnect_enabled && error.retryable {
                    continue;
                }
                return Err(backend_error(error));
            }
        }
    }

    async fn close(&mut self) -> Result<(), BackendError> {
        self.closed = true;
        self.stats.connected.store(false, Ordering::Relaxed);
        if let Some(mut peer) = self.peer.take() {
            let _ = peer.session.peer_closed();
            peer.stream
                .shutdown()
                .await
                .map_err(|_| BackendError::Io("could not close RTMP peer socket".to_owned()))?;
        }
        self.pending.clear();
        Ok(())
    }

    fn observer(&self) -> Option<Arc<dyn PacketSourceObserver>> {
        Some(Arc::new(RtmpObserver {
            state: Arc::clone(&self.stats),
        }))
    }
}

async fn flush_outbound(
    peer: &mut Peer,
    handshake_timeout: Duration,
    read_timeout: Duration,
) -> Result<(), RtmpError> {
    loop {
        let bytes = peer.session.drain_outbound(READ_BUFFER_BYTES);
        if bytes.is_empty() {
            return Ok(());
        }
        let wait = peer_io_timeout(peer, handshake_timeout, read_timeout)?;
        timeout(wait, peer.stream.write_all(&bytes))
            .await
            .map_err(|_| {
                RtmpError::new(
                    RtmpErrorCode::Timeout,
                    RtmpErrorStage::Send,
                    true,
                    "RTMP peer did not accept protocol output before the deadline",
                )
            })?
            .map_err(|_| {
                RtmpError::new(
                    RtmpErrorCode::Io,
                    RtmpErrorStage::Send,
                    true,
                    "RTMP socket write failed",
                )
            })?;
    }
}

fn peer_io_timeout(
    peer: &Peer,
    handshake_timeout: Duration,
    read_timeout: Duration,
) -> Result<Duration, RtmpError> {
    if peer.session.state() == SessionState::Publishing {
        return Ok(read_timeout);
    }
    handshake_timeout
        .checked_sub(peer.accepted_at.elapsed())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| {
            RtmpError::new(
                RtmpErrorCode::Timeout,
                RtmpErrorStage::Receive,
                true,
                "RTMP handshake did not complete before the configured deadline",
            )
        })
}

fn media_error(error: impl fmt::Display) -> RtmpError {
    RtmpError::new(
        RtmpErrorCode::Protocol,
        RtmpErrorStage::Receive,
        true,
        format!("RTMP media payload is unsupported or malformed: {error}"),
    )
}

fn backend_error(error: RtmpError) -> BackendError {
    if error.code == RtmpErrorCode::InvalidEndpoint || error.stage == RtmpErrorStage::Configuration
    {
        BackendError::Unsupported(format!("{:?}: {}", error.code, error.message()))
    } else {
        BackendError::Io(format!("{:?}: {}", error.code, error.message()))
    }
}

#[derive(Debug, Default)]
struct RtmpSourceStats {
    connected: AtomicBool,
    packets_received: AtomicU64,
    reconnects: AtomicU64,
    last_data: Mutex<Option<Instant>>,
}

impl RtmpSourceStats {
    fn record_packet(&self) {
        self.packets_received.fetch_add(1, Ordering::Relaxed);
        *self.last_data.lock().expect("RTMP stats lock poisoned") = Some(Instant::now());
    }

    fn snapshot(&self) -> PacketSourceRuntimeStats {
        PacketSourceRuntimeStats {
            protocol: "rtmp".to_owned(),
            connected: self.connected.load(Ordering::Relaxed),
            transport: "tcp".to_owned(),
            packets_received: self.packets_received.load(Ordering::Relaxed),
            packets_lost: 0,
            reconnects: self.reconnects.load(Ordering::Relaxed),
            last_data_age_ms: self
                .last_data
                .lock()
                .expect("RTMP stats lock poisoned")
                .map(|at| at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64),
        }
    }
}

#[derive(Debug)]
struct RtmpObserver {
    state: Arc<RtmpSourceStats>,
}

#[async_trait]
impl PacketSourceObserver for RtmpObserver {
    async fn stats(&self) -> Result<PacketSourceRuntimeStats, BackendError> {
        Ok(self.state.snapshot())
    }
}

#[cfg(test)]
mod tests {
    use aimedia_core::{
        backend::{CodecId, PacketSource},
        config::ReconnectConfig,
    };
    use bytes::Bytes;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
        time::{Duration, timeout},
    };

    use super::*;
    use crate::{AvcPublisher, PublishSession, RawAudioFrame, SessionState};

    #[test]
    fn timestamp_unwrapper_extends_the_32_bit_rtmp_clock() {
        let mut clock = TimestampUnwrapper::default();
        assert_eq!(clock.unwrap(u32::MAX - 5), i64::from(u32::MAX - 5));
        assert_eq!(clock.unwrap(9), (1_i64 << 32) + 9);
    }

    #[tokio::test]
    async fn listener_converts_a_real_tcp_publish_session_into_runtime_packets() {
        let config = RtmpConfig {
            mode: RtmpMode::Listen,
            stream_name: Some("camera".to_owned()),
            stream_name_ref: None,
            connect_timeout_ms: 3_000,
            handshake_timeout_ms: 3_000,
            read_timeout_ms: 3_000,
            max_message_bytes: 1024 * 1024,
            reconnect: ReconnectConfig::default(),
        };
        let mut source = RtmpPacketSource::bind("rtmp://127.0.0.1:0/live", &config)
            .await
            .unwrap();
        let address = source.local_addr().unwrap();
        let publisher = tokio::spawn(publish_fixture(address));

        let mut packets = Vec::new();
        while packets.len() < 2 {
            packets.push(
                timeout(Duration::from_secs(5), source.receive_packet())
                    .await
                    .expect("RTMP listener timed out")
                    .expect("RTMP listener rejected fixture media"),
            );
        }

        let video = packets
            .iter()
            .find(|packet| packet.codec == CodecId::H264)
            .expect("fixture should contain video");
        assert!(video.keyframe);
        assert_eq!(video.pts.as_millis(), 107);
        assert!(
            video
                .data
                .windows(3)
                .any(|window| window == [0x65, 0x88, 0x84])
        );

        let audio = packets
            .iter()
            .find(|packet| packet.codec == CodecId::AacLc)
            .expect("fixture should contain audio");
        assert_eq!(audio.pts.as_millis(), 120);
        assert_eq!(&audio.data[..2], &[0xff, 0xf1]);

        publisher.await.unwrap();
        let publisher = tokio::spawn(publish_fixture(address));
        let mut reconnected = Vec::new();
        while reconnected.len() < 2 {
            reconnected.push(
                timeout(Duration::from_secs(5), source.receive_packet())
                    .await
                    .expect("RTMP listener reconnect timed out")
                    .expect("RTMP listener rejected the replacement publisher"),
            );
        }
        assert!(reconnected.iter().all(|packet| packet.discontinuity));
        publisher.await.unwrap();

        let stats = source.observer().unwrap().stats().await.unwrap();
        assert_eq!(stats.protocol, "rtmp");
        assert_eq!(stats.packets_received, 4);
        assert_eq!(stats.reconnects, 1);
        source.close().await.unwrap();
    }

    async fn publish_fixture(address: std::net::SocketAddr) {
        let mut stream = TcpStream::connect(address).await.unwrap();
        let mut session =
            PublishSession::new(&format!("rtmp://{address}/live"), "camera", 1024 * 1024).unwrap();
        let mut incoming = [0_u8; 64 * 1024];

        for _ in 0..32 {
            flush_publisher(&mut stream, &mut session).await;
            if session.state() == SessionState::Publishing {
                break;
            }
            let received = timeout(Duration::from_secs(3), stream.read(&mut incoming))
                .await
                .expect("RTMP server response timed out")
                .unwrap();
            assert!(received > 0);
            session.feed(&incoming[..received]).unwrap();
        }
        assert_eq!(session.state(), SessionState::Publishing);

        let annex_b = [
            &[0, 0, 0, 1, 0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9][..],
            &[0, 0, 0, 1, 0x68, 0xee, 0x3c, 0x80][..],
            &[0, 0, 0, 1, 0x65, 0x88, 0x84][..],
        ]
        .concat();
        for frame in AvcPublisher::default()
            .push_annex_b(100, 7, &annex_b)
            .unwrap()
        {
            session.send_video(frame).unwrap();
        }
        session
            .send_audio(RawAudioFrame {
                timestamp_ms: 100,
                aac: true,
                sequence_header: true,
                payload: Bytes::from_static(&[0x11, 0x90]),
            })
            .unwrap();
        session
            .send_audio(RawAudioFrame {
                timestamp_ms: 120,
                aac: true,
                sequence_header: false,
                payload: Bytes::from_static(&[0x11, 0x22, 0x33, 0x44]),
            })
            .unwrap();
        flush_publisher(&mut stream, &mut session).await;
    }

    async fn flush_publisher(stream: &mut TcpStream, session: &mut PublishSession) {
        loop {
            let bytes = session.drain_outbound(64 * 1024);
            if bytes.is_empty() {
                break;
            }
            stream.write_all(&bytes).await.unwrap();
        }
    }
}
