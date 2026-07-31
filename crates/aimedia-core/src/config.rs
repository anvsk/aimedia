use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const API_VERSION: &str = "aimedia/v1alpha1";
pub const KIND: &str = "DirectorPipeline";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read configuration: {0}")]
    Read(#[from] std::io::Error),
    #[error("invalid YAML: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("configuration validation failed: {0:?}")]
    Validation(Vec<String>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PipelineConfig {
    pub api_version: String,
    pub kind: String,
    pub metadata: Metadata,
    pub inputs: Vec<InputConfig>,
    pub output: OutputConfig,
    #[serde(default)]
    pub media: MediaConfig,
    #[serde(default)]
    pub sync: SyncConfig,
    #[serde(default)]
    pub fast_analyzers: FastAnalyzersConfig,
    #[serde(default)]
    pub vlm_advisor: VlmAdvisorConfig,
    #[serde(default)]
    pub director_policy: DirectorPolicyConfig,
    #[serde(default)]
    pub audio_switch: AudioSwitchConfig,
    #[serde(default)]
    pub failure_policy: FailurePolicyConfig,
    #[serde(default)]
    pub control: ControlConfig,
}

impl PipelineConfig {
    pub fn from_yaml_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let contents = fs::read_to_string(path)?;
        Self::from_yaml(&contents)
    }

    pub fn from_yaml(contents: &str) -> Result<Self, ConfigError> {
        let config: Self = serde_yaml::from_str(contents)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut errors = Vec::new();

        if self.api_version != API_VERSION {
            errors.push(format!(
                "apiVersion must be {API_VERSION:?}, got {:?}",
                self.api_version
            ));
        }
        if self.kind != KIND {
            errors.push(format!("kind must be {KIND:?}, got {:?}", self.kind));
        }
        if self.metadata.name.trim().is_empty() {
            errors.push("metadata.name must not be empty".to_owned());
        }
        if self.inputs.len() != 2 {
            errors.push(format!(
                "exactly two inputs are required for v1alpha1, got {}",
                self.inputs.len()
            ));
        }
        if self.sync.master_input > 1 {
            errors.push("sync.masterInput must be 0 or 1".to_owned());
        }

        let mut names = HashSet::new();
        for (index, input) in self.inputs.iter().enumerate() {
            let prefix = format!("inputs[{index}]");
            if input.name.trim().is_empty() {
                errors.push(format!("{prefix}.name must not be empty"));
            } else if !names.insert(input.name.as_str()) {
                errors.push(format!("{prefix}.name {:?} is duplicated", input.name));
            }
            validate_srt_uri(&input.uri, &format!("{prefix}.uri"), &mut errors);
            if !(-5_000..=5_000).contains(&input.offset_ms) {
                errors.push(format!("{prefix}.offsetMs must be between -5000 and 5000"));
            }
            validate_secret_ref(input.secret_ref.as_ref(), &prefix, &mut errors);
            validate_srt(&input.srt, &format!("{prefix}.srt"), &mut errors);
        }

        validate_srt_uri(&self.output.uri, "output.uri", &mut errors);
        validate_secret_ref(self.output.secret_ref.as_ref(), "output", &mut errors);
        validate_srt(&self.output.srt, "output.srt", &mut errors);

        let video = &self.media.video;
        if video.width == 0 || video.width > 1920 {
            errors.push("media.video.width must be in 1..=1920".to_owned());
        }
        if video.height == 0 || video.height > 1080 {
            errors.push("media.video.height must be in 1..=1080".to_owned());
        }
        if video.fps == 0 || video.fps > 30 {
            errors.push("media.video.fps must be in 1..=30 for the alpha profile".to_owned());
        }
        if video.gop_ms < 250 || video.gop_ms > 10_000 {
            errors.push("media.video.gopMs must be between 250 and 10000".to_owned());
        }
        if !video.profile.eq_ignore_ascii_case("main") {
            errors.push("media.video.profile must be main for v1alpha1".to_owned());
        }
        if video.b_frames != 0 {
            errors
                .push("media.video.bFrames must be 0 for the low-latency alpha profile".to_owned());
        }

        let audio = &self.media.audio;
        if audio.sample_rate != 48_000 {
            errors.push("media.audio.sampleRate must be 48000 for v1alpha1".to_owned());
        }
        if audio.channels != 2 {
            errors.push("media.audio.channels must be 2 for v1alpha1".to_owned());
        }

        if self.sync.buffer_ms < 100 || self.sync.buffer_ms > 5_000 {
            errors.push("sync.bufferMs must be between 100 and 5000".to_owned());
        }
        if self.sync.max_skew_ms == 0 || self.sync.max_skew_ms > self.sync.buffer_ms {
            errors.push("sync.maxSkewMs must be in 1..=sync.bufferMs".to_owned());
        }

        let policy = &self.director_policy;
        if !(0.0..=1.0).contains(&policy.score_margin) {
            errors.push("directorPolicy.scoreMargin must be between 0 and 1".to_owned());
        }
        if policy.min_shot_ms < policy.candidate_hold_ms {
            errors.push(
                "directorPolicy.minShotMs must be greater than or equal to candidateHoldMs"
                    .to_owned(),
            );
        }
        if self.vlm_advisor.weight > 0.25 {
            errors.push("vlmAdvisor.weight must not exceed 0.25".to_owned());
        }
        if self.vlm_advisor.valid_for_ms > 3_000 {
            errors.push("vlmAdvisor.validForMs must not exceed 3000".to_owned());
        }
        match self.vlm_advisor.mode {
            VlmMode::Disabled => {}
            VlmMode::Local => {
                if self
                    .vlm_advisor
                    .endpoint
                    .as_deref()
                    .unwrap_or("")
                    .is_empty()
                {
                    errors.push("vlmAdvisor.endpoint is required in local mode".to_owned());
                }
            }
            VlmMode::Remote => {
                if self
                    .vlm_advisor
                    .endpoint
                    .as_deref()
                    .unwrap_or("")
                    .is_empty()
                {
                    errors.push("vlmAdvisor.endpoint is required in remote mode".to_owned());
                }
                if !self.vlm_advisor.explicit_privacy_consent {
                    errors.push(
                        "vlmAdvisor.explicitPrivacyConsent must be true in remote mode".to_owned(),
                    );
                }
                if self
                    .vlm_advisor
                    .token_env
                    .as_deref()
                    .unwrap_or("")
                    .is_empty()
                {
                    errors.push("vlmAdvisor.tokenEnv is required in remote mode".to_owned());
                }
            }
        }

        if self.audio_switch.crossfade_ms == 0 || self.audio_switch.crossfade_ms > 500 {
            errors.push("audioSwitch.crossfadeMs must be in 1..=500".to_owned());
        }
        if !(-30.0..=-8.0).contains(&self.audio_switch.target_lufs) {
            errors.push("audioSwitch.targetLufs must be between -30 and -8".to_owned());
        }
        if !(-12.0..=0.0).contains(&self.audio_switch.true_peak_dbfs) {
            errors.push("audioSwitch.truePeakDbfs must be between -12 and 0".to_owned());
        }

        if self.control.socket_path.as_os_str().is_empty() {
            errors.push("control.socketPath must not be empty".to_owned());
        }
        if parse_socket_mode(&self.control.socket_mode).is_none() {
            errors
                .push("control.socketMode must be an octal mode between 0000 and 0777".to_owned());
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Validation(errors))
        }
    }
}

fn validate_srt(config: &SrtConfig, path: &str, errors: &mut Vec<String>) {
    if !(20..=8_000).contains(&config.latency_ms) {
        errors.push(format!("{path}.latencyMs must be between 20 and 8000"));
    }
    if !(100..=60_000).contains(&config.connect_timeout_ms) {
        errors.push(format!(
            "{path}.connectTimeoutMs must be between 100 and 60000"
        ));
    }
    if config.reconnect.initial_backoff_ms == 0
        || config.reconnect.initial_backoff_ms > config.reconnect.max_backoff_ms
        || config.reconnect.max_backoff_ms > 60_000
    {
        errors.push(format!(
            "{path}.reconnect backoff must satisfy 1 <= initialBackoffMs <= maxBackoffMs <= 60000"
        ));
    }
    if !matches!(config.key_length, 16 | 24 | 32) {
        errors.push(format!("{path}.keyLength must be 16, 24, or 32"));
    }
    if config.stream_id.is_some() && config.stream_id_ref.is_some() {
        errors.push(format!("{path} must not set both streamId and streamIdRef"));
    }
    if config
        .stream_id
        .as_deref()
        .is_some_and(contains_sensitive_stream_id)
    {
        errors.push(format!(
            "{path}.streamId appears to contain a token or credential; use streamIdRef"
        ));
    }
    validate_secret_ref(
        config.stream_id_ref.as_ref(),
        &format!("{path}.streamIdRef"),
        errors,
    );
}

fn contains_sensitive_stream_id(stream_id: &str) -> bool {
    let lower = stream_id.to_ascii_lowercase();
    ["token=", "secret=", "password=", "passphrase=", "bearer "]
        .iter()
        .any(|marker| lower.contains(marker))
}

#[must_use]
pub fn parse_socket_mode(value: &str) -> Option<u32> {
    let mode = u32::from_str_radix(value, 8).ok()?;
    (mode <= 0o777).then_some(mode)
}

fn validate_srt_uri(uri: &str, path: &str, errors: &mut Vec<String>) {
    if !uri.starts_with("srt://") {
        errors.push(format!("{path} must use the srt:// scheme"));
    }
    let authority = uri
        .strip_prefix("srt://")
        .and_then(|rest| rest.split(['/', '?']).next())
        .unwrap_or_default();
    if authority.contains('@') {
        errors.push(format!(
            "{path} contains URI userinfo; use secretRef instead"
        ));
    }
    let lower = uri.to_ascii_lowercase();
    for sensitive in ["passphrase=", "token=", "secret=", "password="] {
        if lower.contains(sensitive) {
            errors.push(format!(
                "{path} contains inline credentials; use secretRef instead"
            ));
            break;
        }
    }
}

fn validate_secret_ref(secret_ref: Option<&SecretRef>, path: &str, errors: &mut Vec<String>) {
    let Some(secret_ref) = secret_ref else {
        return;
    };
    let env_present = secret_ref
        .env
        .as_deref()
        .is_some_and(|value| !value.is_empty());
    let file_present = secret_ref
        .file
        .as_deref()
        .is_some_and(|value| !value.as_os_str().is_empty());
    if env_present == file_present {
        errors.push(format!(
            "{path}.secretRef must set exactly one of env or file"
        ));
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Metadata {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InputConfig {
    pub name: String,
    #[serde(default)]
    pub role: CameraRole,
    pub uri: String,
    #[serde(default)]
    pub offset_ms: i64,
    #[serde(default)]
    pub secret_ref: Option<SecretRef>,
    #[serde(default)]
    pub srt: SrtConfig,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CameraRole {
    Wide,
    Close,
    #[default]
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OutputConfig {
    pub uri: String,
    #[serde(default)]
    pub secret_ref: Option<SecretRef>,
    #[serde(default)]
    pub srt: SrtConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SecretRef {
    #[serde(default)]
    pub env: Option<String>,
    #[serde(default)]
    pub file: Option<std::path::PathBuf>,
}

impl SecretRef {
    pub fn resolve(&self) -> Result<String, SecretError> {
        match (&self.env, &self.file) {
            (Some(name), None) if !name.is_empty() => {
                std::env::var(name).map_err(|_| SecretError::MissingEnvironment(name.clone()))
            }
            (None, Some(path)) if !path.as_os_str().is_empty() => {
                let value = fs::read_to_string(path)
                    .map_err(|source| SecretError::ReadFile(path.clone(), source))?;
                Ok(value.trim_end_matches(['\r', '\n']).to_owned())
            }
            _ => Err(SecretError::InvalidReference),
        }
    }
}

#[derive(Debug, Error)]
pub enum SecretError {
    #[error("secret environment variable {0:?} is not set")]
    MissingEnvironment(String),
    #[error("failed to read secret file {0}: {1}")]
    ReadFile(PathBuf, std::io::Error),
    #[error("secret reference must set exactly one of env or file")]
    InvalidReference,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SrtConfig {
    #[serde(default)]
    pub mode: Option<SrtMode>,
    #[serde(default = "default_srt_latency")]
    pub latency_ms: u64,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_ms: u64,
    #[serde(default)]
    pub reconnect: ReconnectConfig,
    #[serde(default)]
    pub stream_id: Option<String>,
    #[serde(default)]
    pub stream_id_ref: Option<SecretRef>,
    #[serde(default = "default_srt_key_length")]
    pub key_length: u16,
}

impl Default for SrtConfig {
    fn default() -> Self {
        Self {
            mode: None,
            latency_ms: default_srt_latency(),
            connect_timeout_ms: default_connect_timeout(),
            reconnect: ReconnectConfig::default(),
            stream_id: None,
            stream_id_ref: None,
            key_length: default_srt_key_length(),
        }
    }
}

impl SrtConfig {
    #[must_use]
    pub fn effective_mode(&self, uri: &str) -> SrtMode {
        self.mode
            .or_else(|| mode_from_uri_query(uri))
            .unwrap_or(SrtMode::Caller)
    }

    pub fn resolve_stream_id(&self) -> Result<Option<String>, SecretError> {
        match (&self.stream_id, &self.stream_id_ref) {
            (Some(value), None) => Ok(Some(value.clone())),
            (None, Some(reference)) => reference.resolve().map(Some),
            (None, None) => Ok(None),
            _ => Err(SecretError::InvalidReference),
        }
    }
}

fn mode_from_uri_query(uri: &str) -> Option<SrtMode> {
    let query = uri.split_once('?')?.1;
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        if key.eq_ignore_ascii_case("mode") {
            if value.eq_ignore_ascii_case("caller") {
                Some(SrtMode::Caller)
            } else if value.eq_ignore_ascii_case("listener") {
                Some(SrtMode::Listener)
            } else {
                None
            }
        } else {
            None
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SrtMode {
    Caller,
    Listener,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReconnectConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_initial_backoff")]
    pub initial_backoff_ms: u64,
    #[serde(default = "default_max_backoff")]
    pub max_backoff_ms: u64,
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            initial_backoff_ms: default_initial_backoff(),
            max_backoff_ms: default_max_backoff(),
        }
    }
}

const fn default_srt_latency() -> u64 {
    120
}
const fn default_connect_timeout() -> u64 {
    3_000
}
const fn default_srt_key_length() -> u16 {
    16
}
const fn default_initial_backoff() -> u64 {
    250
}
const fn default_max_backoff() -> u64 {
    5_000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ControlConfig {
    #[serde(default = "default_control_socket")]
    pub socket_path: PathBuf,
    #[serde(default = "default_control_mode")]
    pub socket_mode: String,
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self {
            socket_path: default_control_socket(),
            socket_mode: default_control_mode(),
        }
    }
}

fn default_control_socket() -> PathBuf {
    PathBuf::from("/run/aimedia/aimedia.sock")
}

fn default_control_mode() -> String {
    "0660".to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaConfig {
    #[serde(default)]
    pub video: VideoConfig,
    #[serde(default)]
    pub audio: AudioConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VideoConfig {
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,
    #[serde(default = "default_fps")]
    pub fps: u32,
    #[serde(default = "default_video_bitrate")]
    pub bitrate_kbps: u32,
    #[serde(default = "default_gop_ms")]
    pub gop_ms: u64,
    #[serde(default = "default_h264_profile")]
    pub profile: String,
    #[serde(default)]
    pub b_frames: u8,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            width: default_width(),
            height: default_height(),
            fps: default_fps(),
            bitrate_kbps: default_video_bitrate(),
            gop_ms: default_gop_ms(),
            profile: default_h264_profile(),
            b_frames: 0,
        }
    }
}

const fn default_width() -> u32 {
    1920
}
const fn default_height() -> u32 {
    1080
}
const fn default_fps() -> u32 {
    30
}
const fn default_video_bitrate() -> u32 {
    6_000
}
const fn default_gop_ms() -> u64 {
    1_000
}
fn default_h264_profile() -> String {
    "main".to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AudioConfig {
    #[serde(default = "default_audio_rate")]
    pub sample_rate: u32,
    #[serde(default = "default_channels")]
    pub channels: u8,
    #[serde(default = "default_audio_bitrate")]
    pub bitrate_kbps: u32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            sample_rate: default_audio_rate(),
            channels: default_channels(),
            bitrate_kbps: default_audio_bitrate(),
        }
    }
}

const fn default_audio_rate() -> u32 {
    48_000
}
const fn default_channels() -> u8 {
    2
}
const fn default_audio_bitrate() -> u32 {
    128
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SyncConfig {
    #[serde(default = "default_master_input")]
    pub master_input: usize,
    #[serde(default = "default_sync_buffer")]
    pub buffer_ms: u64,
    #[serde(default = "default_max_skew")]
    pub max_skew_ms: u64,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            master_input: default_master_input(),
            buffer_ms: default_sync_buffer(),
            max_skew_ms: default_max_skew(),
        }
    }
}

const fn default_master_input() -> usize {
    0
}
const fn default_sync_buffer() -> u64 {
    1_000
}
const fn default_max_skew() -> u64 {
    80
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FastAnalyzersConfig {
    #[serde(default = "default_true")]
    pub vad: bool,
    #[serde(default = "default_true")]
    pub person: bool,
    #[serde(default = "default_true")]
    pub mouth_motion: bool,
    #[serde(default = "default_true")]
    pub quality: bool,
}

impl Default for FastAnalyzersConfig {
    fn default() -> Self {
        Self {
            vad: true,
            person: true,
            mouth_motion: true,
            quality: true,
        }
    }
}

const fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VlmAdvisorConfig {
    #[serde(default)]
    pub mode: VlmMode,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default = "default_vlm_model")]
    pub model: String,
    #[serde(default = "default_vlm_interval")]
    pub interval_ms: u64,
    #[serde(default = "default_vlm_deadline")]
    pub deadline_ms: u64,
    #[serde(default = "default_vlm_validity")]
    pub valid_for_ms: u64,
    #[serde(default = "default_vlm_weight")]
    pub weight: f32,
    #[serde(default)]
    pub token_env: Option<String>,
    #[serde(default)]
    pub explicit_privacy_consent: bool,
}

impl Default for VlmAdvisorConfig {
    fn default() -> Self {
        Self {
            mode: VlmMode::Disabled,
            endpoint: None,
            model: default_vlm_model(),
            interval_ms: default_vlm_interval(),
            deadline_ms: default_vlm_deadline(),
            valid_for_ms: default_vlm_validity(),
            weight: default_vlm_weight(),
            token_env: None,
            explicit_privacy_consent: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum VlmMode {
    #[default]
    Disabled,
    Local,
    Remote,
}

fn default_vlm_model() -> String {
    "Qwen3-VL-2B-Instruct".to_owned()
}
const fn default_vlm_interval() -> u64 {
    2_000
}
const fn default_vlm_deadline() -> u64 {
    800
}
const fn default_vlm_validity() -> u64 {
    3_000
}
const fn default_vlm_weight() -> f32 {
    0.25
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DirectorPolicyConfig {
    #[serde(default = "default_min_shot")]
    pub min_shot_ms: u64,
    #[serde(default = "default_score_margin")]
    pub score_margin: f32,
    #[serde(default = "default_candidate_hold")]
    pub candidate_hold_ms: u64,
    #[serde(default = "default_cooldown")]
    pub cooldown_ms: u64,
}

impl Default for DirectorPolicyConfig {
    fn default() -> Self {
        Self {
            min_shot_ms: default_min_shot(),
            score_margin: default_score_margin(),
            candidate_hold_ms: default_candidate_hold(),
            cooldown_ms: default_cooldown(),
        }
    }
}

const fn default_min_shot() -> u64 {
    3_000
}
const fn default_score_margin() -> f32 {
    0.15
}
const fn default_candidate_hold() -> u64 {
    800
}
const fn default_cooldown() -> u64 {
    2_000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AudioSwitchConfig {
    #[serde(default = "default_target_lufs")]
    pub target_lufs: f32,
    #[serde(default = "default_crossfade")]
    pub crossfade_ms: u64,
    #[serde(default = "default_peak")]
    pub true_peak_dbfs: f32,
}

impl Default for AudioSwitchConfig {
    fn default() -> Self {
        Self {
            target_lufs: default_target_lufs(),
            crossfade_ms: default_crossfade(),
            true_peak_dbfs: default_peak(),
        }
    }
}

const fn default_target_lufs() -> f32 {
    -16.0
}
const fn default_crossfade() -> u64 {
    80
}
const fn default_peak() -> f32 {
    -1.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FailurePolicyConfig {
    #[serde(default = "default_true")]
    pub failover_on_disconnect: bool,
    #[serde(default = "default_true")]
    pub pause_auto_on_skew: bool,
    #[serde(default = "default_true")]
    pub ignore_vlm_failure: bool,
}

impl Default for FailurePolicyConfig {
    fn default() -> Self {
        Self {
            failover_on_disconnect: true,
            pause_auto_on_skew: true,
            ignore_vlm_failure: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PipelineConfig, validate_srt_uri};

    #[test]
    fn accepts_the_reference_alpha_configuration() {
        let yaml = include_str!("../../../examples/director.yaml");
        let config = PipelineConfig::from_yaml(yaml).expect("reference configuration is valid");
        assert_eq!(config.inputs.len(), 2);
        assert_eq!(config.sync.max_skew_ms, 80);
        assert_eq!(config.media.video.gop_ms, 1_000);
    }

    #[test]
    fn rejects_query_and_userinfo_credentials_in_srt_uris() {
        for uri in [
            "srt://user:password@example.test:9000",
            "srt://example.test:9000?token=secret",
        ] {
            let mut errors = Vec::new();
            validate_srt_uri(uri, "input.uri", &mut errors);
            assert!(!errors.is_empty(), "{uri} must be rejected");
        }
    }
}
