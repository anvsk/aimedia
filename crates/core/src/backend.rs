use std::{any::Any, fmt, sync::Arc};

use async_trait::async_trait;
use bytes::Bytes;
use thiserror::Error;

use crate::{
    control::{GpuSurfaceRuntimeStats, SrtRuntimeStats},
    director::FastSignals,
    time::Timestamp,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecId {
    H264,
    H265,
    AacLc,
    G711Alaw,
    G711Mulaw,
    PcmF32,
    Unknown(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryDomain {
    Cpu,
    Cuda { device: u32 },
    DmaBuf,
}

#[derive(Debug, Clone)]
pub struct MediaPacket {
    pub stream_id: u32,
    pub codec: CodecId,
    pub pts: Timestamp,
    pub dts: Option<Timestamp>,
    pub duration: Option<Timestamp>,
    pub keyframe: bool,
    pub discontinuity: bool,
    pub data: Bytes,
}

pub trait SurfaceLease: Send + Sync {
    /// Opaque backend handle. Only the backend that created the lease may interpret it.
    fn handle(&self) -> u64;

    /// Lets a backend recover its own typed metadata without exposing it to the media core.
    fn as_any(&self) -> &dyn Any;
}

#[derive(Clone)]
pub struct VideoSurface {
    lease: Arc<dyn SurfaceLease>,
}

impl VideoSurface {
    #[must_use]
    pub fn new(lease: impl SurfaceLease + 'static) -> Self {
        Self {
            lease: Arc::new(lease),
        }
    }

    #[must_use]
    pub fn handle(&self) -> u64 {
        self.lease.handle()
    }

    #[must_use]
    pub fn downcast_ref<T: SurfaceLease + 'static>(&self) -> Option<&T> {
        self.lease.as_any().downcast_ref()
    }
}

impl fmt::Debug for VideoSurface {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VideoSurface")
            .field("handle", &format_args!("0x{:x}", self.handle()))
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub pts: Timestamp,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub memory: MemoryDomain,
    /// Backend-owned RAII lease. The core can copy the lease but never dereferences the handle.
    pub surface: VideoSurface,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Nv12,
    Yuv420p,
    Rgba,
}

#[derive(Debug, Clone)]
pub struct AudioFrame {
    pub pts: Timestamp,
    pub sample_rate: u32,
    pub channels: u8,
    pub interleaved: Vec<f32>,
}

#[derive(Debug, Error)]
pub enum BackendError {
    #[error("backend is not available in this build: {0}")]
    Unavailable(String),
    #[error("backend input ended")]
    EndOfStream,
    #[error("backend rejected media format: {0}")]
    Unsupported(String),
    #[error("backend I/O failed: {0}")]
    Io(String),
    #[error("backend processing failed: {0}")]
    Processing(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportChunk {
    pub data: Vec<u8>,
    pub discontinuity: bool,
}

#[async_trait]
pub trait TransportObserver: Send + Sync {
    async fn stats(&self) -> Result<SrtRuntimeStats, BackendError>;
}

#[async_trait]
pub trait Transport: Send {
    async fn receive(&mut self) -> Result<TransportChunk, BackendError>;
    async fn send(&mut self, payload: &[u8]) -> Result<(), BackendError>;
    async fn close(&mut self) -> Result<(), BackendError>;

    fn observer(&self) -> Option<Arc<dyn TransportObserver>> {
        None
    }
}

/// Pull-based source for protocols that already expose codec access units.
///
/// SRT/MPEG-TS uses [`Transport`] plus the streaming TS demuxer. RTSP/RTP performs
/// depacketization inside its protocol adapter and enters the runtime through this trait instead
/// of being serialized into a synthetic transport stream.
#[async_trait]
pub trait PacketSource: Send {
    async fn receive_packet(&mut self) -> Result<MediaPacket, BackendError>;
    async fn close(&mut self) -> Result<(), BackendError>;

    fn observer(&self) -> Option<Arc<dyn PacketSourceObserver>> {
        None
    }
}

#[async_trait]
pub trait PacketSourceObserver: Send + Sync {
    async fn stats(&self) -> Result<PacketSourceRuntimeStats, BackendError>;
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PacketSourceRuntimeStats {
    pub protocol: String,
    pub connected: bool,
    pub transport: String,
    pub packets_received: u64,
    pub packets_lost: u64,
    pub reconnects: u64,
    pub last_data_age_ms: Option<u64>,
}

pub trait GpuSurfaceObserver: Send + Sync {
    fn stats(&self) -> GpuSurfaceRuntimeStats;
}

pub trait Demuxer: Send {
    fn push(&mut self, payload: &[u8]) -> Result<Vec<MediaPacket>, BackendError>;
    fn flush(&mut self) -> Result<Vec<MediaPacket>, BackendError>;
}

pub trait Muxer: Send {
    fn push(&mut self, packet: &MediaPacket) -> Result<Vec<u8>, BackendError>;
    fn flush(&mut self) -> Result<Vec<u8>, BackendError>;
}

#[async_trait]
pub trait VideoDecoder: Send {
    async fn decode(&mut self, packet: MediaPacket) -> Result<Vec<VideoFrame>, BackendError>;
    async fn flush(&mut self) -> Result<Vec<VideoFrame>, BackendError>;

    fn surface_observer(&self) -> Option<Arc<dyn GpuSurfaceObserver>> {
        None
    }
}

#[async_trait]
pub trait VideoEncoder: Send {
    async fn encode(
        &mut self,
        frame: VideoFrame,
        force_idr: bool,
    ) -> Result<Vec<MediaPacket>, BackendError>;
    async fn flush(&mut self) -> Result<Vec<MediaPacket>, BackendError>;
}

#[async_trait]
pub trait AudioDecoder: Send {
    async fn decode(&mut self, packet: MediaPacket) -> Result<Vec<AudioFrame>, BackendError>;

    async fn flush(&mut self) -> Result<Vec<AudioFrame>, BackendError> {
        Ok(Vec::new())
    }
}

#[async_trait]
pub trait AudioEncoder: Send {
    async fn encode(&mut self, frame: AudioFrame) -> Result<Vec<MediaPacket>, BackendError>;

    async fn flush(&mut self) -> Result<Vec<MediaPacket>, BackendError> {
        Ok(Vec::new())
    }
}

#[async_trait]
pub trait FastAnalyzer: Send + Sync {
    async fn analyze(
        &self,
        video: Option<&VideoFrame>,
        audio: Option<&AudioFrame>,
    ) -> Result<FastSignals, BackendError>;
}
