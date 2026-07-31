//! Core contracts and deterministic scheduling for the aimedia live director.
//!
//! This crate deliberately contains no FFmpeg bindings. Media transports, codecs, inference
//! engines, and hardware accelerators attach through explicit backend traits.

pub mod audio;
pub mod backend;
pub mod config;
pub mod director;
pub mod plugin_abi;
pub mod sync;
pub mod time;
pub mod vlm;

pub use config::{ConfigError, PipelineConfig};
pub use director::{
    CameraSnapshot, Director, DirectorDecision, DirectorEvent, FastSignals, SwitchReason,
};
pub use time::Timestamp;
