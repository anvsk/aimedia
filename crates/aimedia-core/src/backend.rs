use async_trait::async_trait;
use thiserror::Error;

use crate::{director::FastSignals, time::Timestamp};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecId {
    H264,
    AacLc,
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
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub pts: Timestamp,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub memory: MemoryDomain,
    /// Backend-owned handle. The core never dereferences it.
    pub surface_handle: u64,
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

#[async_trait]
pub trait Transport: Send {
    async fn receive(&mut self) -> Result<Vec<u8>, BackendError>;
    async fn send(&mut self, payload: &[u8]) -> Result<(), BackendError>;
    async fn close(&mut self) -> Result<(), BackendError>;
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
}

#[async_trait]
pub trait AudioEncoder: Send {
    async fn encode(&mut self, frame: AudioFrame) -> Result<Vec<MediaPacket>, BackendError>;
}

#[async_trait]
pub trait FastAnalyzer: Send + Sync {
    async fn analyze(
        &self,
        video: Option<&VideoFrame>,
        audio: Option<&AudioFrame>,
    ) -> Result<FastSignals, BackendError>;
}
