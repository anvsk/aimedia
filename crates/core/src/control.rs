use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub const CONTROL_API_VERSION: &str = "aimedia.control/v1alpha1";

#[derive(Debug, Clone)]
pub struct ControlRequest {
    pub api_version: String,
    pub request_id: String,
    pub command: ControlCommand,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "camelCase", deny_unknown_fields)]
enum ControlRequestWire {
    Take {
        #[serde(rename = "apiVersion")]
        api_version: String,
        #[serde(rename = "requestId")]
        request_id: String,
        input: String,
        #[serde(rename = "holdMs", default = "default_hold_ms")]
        hold_ms: u64,
    },
    Auto {
        #[serde(rename = "apiVersion")]
        api_version: String,
        #[serde(rename = "requestId")]
        request_id: String,
    },
    State {
        #[serde(rename = "apiVersion")]
        api_version: String,
        #[serde(rename = "requestId")]
        request_id: String,
    },
}

impl Serialize for ControlRequest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let wire = match &self.command {
            ControlCommand::Take { input, hold_ms } => ControlRequestWire::Take {
                api_version: self.api_version.clone(),
                request_id: self.request_id.clone(),
                input: input.clone(),
                hold_ms: *hold_ms,
            },
            ControlCommand::Auto => ControlRequestWire::Auto {
                api_version: self.api_version.clone(),
                request_id: self.request_id.clone(),
            },
            ControlCommand::State => ControlRequestWire::State {
                api_version: self.api_version.clone(),
                request_id: self.request_id.clone(),
            },
        };
        wire.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ControlRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ControlRequestWire::deserialize(deserializer)?;
        Ok(match wire {
            ControlRequestWire::Take {
                api_version,
                request_id,
                input,
                hold_ms,
            } => Self {
                api_version,
                request_id,
                command: ControlCommand::Take { input, hold_ms },
            },
            ControlRequestWire::Auto {
                api_version,
                request_id,
            } => Self {
                api_version,
                request_id,
                command: ControlCommand::Auto,
            },
            ControlRequestWire::State {
                api_version,
                request_id,
            } => Self {
                api_version,
                request_id,
                command: ControlCommand::State,
            },
        })
    }
}

impl ControlRequest {
    #[must_use]
    pub fn take(request_id: impl Into<String>, input: impl Into<String>, hold_ms: u64) -> Self {
        Self {
            api_version: CONTROL_API_VERSION.to_owned(),
            request_id: request_id.into(),
            command: ControlCommand::Take {
                input: input.into(),
                hold_ms,
            },
        }
    }

    #[must_use]
    pub fn auto(request_id: impl Into<String>) -> Self {
        Self {
            api_version: CONTROL_API_VERSION.to_owned(),
            request_id: request_id.into(),
            command: ControlCommand::Auto,
        }
    }

    #[must_use]
    pub fn state(request_id: impl Into<String>) -> Self {
        Self {
            api_version: CONTROL_API_VERSION.to_owned(),
            request_id: request_id.into(),
            command: ControlCommand::State,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "camelCase", deny_unknown_fields)]
pub enum ControlCommand {
    Take {
        input: String,
        #[serde(default = "default_hold_ms")]
        #[serde(rename = "holdMs")]
        hold_ms: u64,
    },
    Auto,
    State,
}

const fn default_hold_ms() -> u64 {
    5_000
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ControlErrorCode {
    InvalidRequest,
    UnsupportedVersion,
    NotApplicable,
    UnknownInput,
    InvalidHold,
    TargetUnavailable,
    PipelineUnavailable,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ControlResponse {
    pub api_version: String,
    pub request_id: String,
    pub accepted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<ControlErrorCode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<PipelineRuntimeState>,
}

impl ControlResponse {
    #[must_use]
    pub fn accepted(request_id: impl Into<String>, state: PipelineRuntimeState) -> Self {
        Self {
            api_version: CONTROL_API_VERSION.to_owned(),
            request_id: request_id.into(),
            accepted: true,
            error_code: None,
            message: None,
            state: Some(state),
        }
    }

    #[must_use]
    pub fn rejected(
        request_id: impl Into<String>,
        error_code: ControlErrorCode,
        message: impl Into<String>,
        state: Option<PipelineRuntimeState>,
    ) -> Self {
        Self {
            api_version: CONTROL_API_VERSION.to_owned(),
            request_id: request_id.into(),
            accepted: false,
            error_code: Some(error_code),
            message: Some(message.into()),
            state,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PipelineMode {
    Single,
    Automatic,
    Manual,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SrtRuntimeStats {
    pub connected: bool,
    pub rtt_ms: f64,
    pub packets_lost: u64,
    pub packets_retransmitted: u64,
    pub receive_buffer_bytes: u64,
    pub reconnects: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_data_age_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RtspRuntimeStats {
    pub connected: bool,
    pub transport: String,
    pub packets_received: u64,
    pub packets_lost: u64,
    pub reconnects: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_data_age_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RtmpRuntimeStats {
    pub connected: bool,
    pub transport: String,
    pub packets_received: u64,
    pub reconnects: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_data_age_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InputRuntimeState {
    pub name: String,
    pub healthy: bool,
    pub synchronized: bool,
    pub frozen: bool,
    pub skew_ms: u64,
    pub video_timeline_depth: usize,
    pub audio_timeline_depth: usize,
    pub srt: SrtRuntimeStats,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtsp: Option<RtspRuntimeStats>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtmp: Option<RtmpRuntimeStats>,
    #[serde(default)]
    pub codec: InputCodecRuntimeStats,
    #[serde(default)]
    pub gpu: GpuSurfaceRuntimeStats,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InputCodecRuntimeStats {
    pub video_decoded_frames: u64,
    pub audio_decoded_frames: u64,
    pub video_dropped_frames: u64,
    pub audio_dropped_frames: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GpuSurfaceRuntimeStats {
    pub in_use: usize,
    pub capacity: usize,
    pub high_watermark: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LatencyRuntimeStats {
    pub samples: u64,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub max_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OutputRuntimeState {
    pub video_encoded_frames: u64,
    pub audio_encoded_frames: u64,
    pub video_dropped_frames: u64,
    pub audio_dropped_frames: u64,
    pub engine_latency: LatencyRuntimeStats,
    pub srt: SrtRuntimeStats,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QueueRuntimeState {
    pub name: String,
    pub from: String,
    pub to: String,
    pub full_policy: String,
    pub depth: usize,
    pub capacity: usize,
    pub high_watermark: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PipelineRuntimeState {
    pub pipeline: String,
    pub running: bool,
    pub active_input: usize,
    pub active_name: String,
    pub mode: PipelineMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hold_until_ms: Option<u64>,
    pub inputs: Vec<InputRuntimeState>,
    pub output: OutputRuntimeState,
    pub queues: Vec<QueueRuntimeState>,
    pub last_reason: String,
}

#[cfg(test)]
mod tests {
    use super::{CONTROL_API_VERSION, ControlCommand, ControlRequest};

    #[test]
    fn control_request_uses_flat_versioned_json() {
        let request = ControlRequest::take("req-1", "close", 5_000);
        let json = serde_json::to_value(request).expect("request serializes");
        assert_eq!(json["apiVersion"], CONTROL_API_VERSION);
        assert_eq!(json["command"], "take");
        assert_eq!(json["input"], "close");
        assert_eq!(json["holdMs"], 5_000);

        let parsed: ControlRequest = serde_json::from_value(json).expect("request parses");
        assert!(matches!(parsed.command, ControlCommand::Take { .. }));
    }
}
