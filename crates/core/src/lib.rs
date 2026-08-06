//! Core media, time, control, and extension contracts for the aimedia runtime.
//!
//! This crate deliberately contains no FFmpeg bindings. Media transports, codecs, inference
//! engines, and hardware accelerators attach through explicit backend traits.

pub mod audio;
pub mod backend;
pub mod config;
pub mod control;
pub mod director;
pub mod plugin_abi;
pub mod sync;
pub mod time;
pub mod vlm;

pub use config::{ConfigError, MediaJob, PipelineConfig, convert_legacy_yaml};
pub use control::{
    ControlCommand, ControlErrorCode, ControlRequest, ControlResponse, GpuSurfaceRuntimeStats,
    InputCodecRuntimeStats, InputRuntimeState, LatencyRuntimeStats, OutputRuntimeState,
    PipelineMode, PipelineRuntimeState, QueueRuntimeState, RtmpOutputRuntimeStats,
    RtmpRuntimeStats, RtspRuntimeStats, SrtRuntimeStats,
};
pub use director::{
    CameraSnapshot, Director, DirectorDecision, DirectorEvent, FastSignals, SwitchReason,
};
pub use time::Timestamp;
