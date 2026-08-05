use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const API_VERSION: &str = "aimedia/v1alpha2";
pub const KIND: &str = "MediaJob";
pub const LEGACY_API_VERSION: &str = "aimedia/v1alpha1";
pub const LEGACY_KIND: &str = "DirectorPipeline";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read configuration: {0}")]
    Read(#[from] std::io::Error),
    #[error("invalid YAML: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("configuration validation failed: {0:?}")]
    Validation(Vec<String>),
    #[error(
        "legacy DirectorPipeline configuration is not accepted by run/explain; convert it with `aimedia config convert -f <legacy.yaml>`"
    )]
    LegacyRequiresConversion,
}

#[derive(Debug, Clone, Serialize)]
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
        let header: ConfigHeader = serde_yaml::from_str(contents)?;
        if header.api_version == LEGACY_API_VERSION && header.kind == LEGACY_KIND {
            return Err(ConfigError::LegacyRequiresConversion);
        }
        let job: MediaJob = serde_yaml::from_str(contents)?;
        let config = job.into_pipeline_config()?;
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
        if !(1..=2).contains(&self.inputs.len()) {
            errors.push(format!(
                "one or two inputs are required for v1alpha2, got {}",
                self.inputs.len()
            ));
        }
        if self.sync.master_input >= self.inputs.len() {
            errors.push(format!(
                "processing.timing.masterInput must reference a configured input, got {} for {} inputs",
                self.sync.master_input,
                self.inputs.len()
            ));
        }

        let mut names = HashSet::new();
        for (index, input) in self.inputs.iter().enumerate() {
            let prefix = format!("inputs[{index}]");
            if input.name.trim().is_empty() {
                errors.push(format!("{prefix}.name must not be empty"));
            } else if !names.insert(input.name.as_str()) {
                errors.push(format!("{prefix}.name {:?} is duplicated", input.name));
            }
            validate_input_transport(input, &prefix, &mut errors);
            if !(-5_000..=5_000).contains(&input.offset_ms) {
                errors.push(format!("{prefix}.offsetMs must be between -5000 and 5000"));
            }
        }

        validate_srt_uri(&self.output.uri, "outputs[0].uri", &mut errors);
        validate_secret_ref(
            self.output.secret_ref.as_ref(),
            "outputs[0].secretRef",
            &mut errors,
        );
        validate_srt(&self.output.srt, "outputs[0].srt", &mut errors);

        let video = &self.media.video;
        if video.width == 0 || video.width > 1920 {
            errors.push("processing.video.width must be in 1..=1920".to_owned());
        }
        if video.height == 0 || video.height > 1080 {
            errors.push("processing.video.height must be in 1..=1080".to_owned());
        }
        if video.fps == 0 || video.fps > 30 {
            errors.push("processing.video.fps must be in 1..=30 for the alpha profile".to_owned());
        }
        if video.gop_ms < 250 || video.gop_ms > 10_000 {
            errors.push("processing.video.gopMs must be between 250 and 10000".to_owned());
        }
        if !video.profile.eq_ignore_ascii_case("main") {
            errors.push("processing.video.profile must be main for v1alpha2".to_owned());
        }
        if video.b_frames != 0 {
            errors.push(
                "processing.video.bFrames must be 0 for the low-latency alpha profile".to_owned(),
            );
        }

        let audio = &self.media.audio;
        if audio.sample_rate != 48_000 {
            errors.push("processing.audio.sampleRate must be 48000 for v1alpha2".to_owned());
        }
        if audio.channels != 2 {
            errors.push("processing.audio.channels must be 2 for v1alpha2".to_owned());
        }

        if self.sync.buffer_ms < 100 || self.sync.buffer_ms > 5_000 {
            errors.push("processing.timing.bufferMs must be between 100 and 5000".to_owned());
        }
        if self.sync.max_skew_ms == 0 || self.sync.max_skew_ms > self.sync.buffer_ms {
            errors.push(
                "processing.timing.maxSkewMs must be in 1..=processing.timing.bufferMs".to_owned(),
            );
        }

        let policy = &self.director_policy;
        if !(0.0..=1.0).contains(&policy.score_margin) {
            errors
                .push("processing.switching.policy.scoreMargin must be between 0 and 1".to_owned());
        }
        if policy.min_shot_ms < policy.candidate_hold_ms {
            errors.push(
                "processing.switching.policy.minShotMs must be greater than or equal to candidateHoldMs"
                    .to_owned(),
            );
        }
        if self.vlm_advisor.weight > 0.25 {
            errors.push("taps[0].vlmAdvisor.weight must not exceed 0.25".to_owned());
        }
        if self.vlm_advisor.valid_for_ms > 3_000 {
            errors.push("taps[0].vlmAdvisor.validForMs must not exceed 3000".to_owned());
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
                    errors.push("taps[0].vlmAdvisor.endpoint is required in local mode".to_owned());
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
                    errors
                        .push("taps[0].vlmAdvisor.endpoint is required in remote mode".to_owned());
                }
                if !self.vlm_advisor.explicit_privacy_consent {
                    errors.push(
                        "taps[0].vlmAdvisor.explicitPrivacyConsent must be true in remote mode"
                            .to_owned(),
                    );
                }
                if self
                    .vlm_advisor
                    .token_env
                    .as_deref()
                    .unwrap_or("")
                    .is_empty()
                {
                    errors
                        .push("taps[0].vlmAdvisor.tokenEnv is required in remote mode".to_owned());
                }
            }
        }

        if self.audio_switch.crossfade_ms == 0 || self.audio_switch.crossfade_ms > 500 {
            errors.push("processing.switching.audio.crossfadeMs must be in 1..=500".to_owned());
        }
        if !(-30.0..=-8.0).contains(&self.audio_switch.target_lufs) {
            errors.push(
                "processing.switching.audio.targetLufs must be between -30 and -8".to_owned(),
            );
        }
        if !(-12.0..=0.0).contains(&self.audio_switch.true_peak_dbfs) {
            errors.push(
                "processing.switching.audio.truePeakDbfs must be between -12 and 0".to_owned(),
            );
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

fn validate_input_transport(input: &InputConfig, path: &str, errors: &mut Vec<String>) {
    if has_scheme(&input.uri, "srt://") {
        if input.rtsp.is_some() {
            errors.push(format!("{path}.rtsp must not be set for an srt:// input"));
        }
        validate_srt_uri(&input.uri, &format!("{path}.uri"), errors);
        validate_secret_ref(
            input.secret_ref.as_ref(),
            &format!("{path}.secretRef"),
            errors,
        );
        validate_srt(&input.srt, &format!("{path}.srt"), errors);
        return;
    }

    if has_scheme(&input.uri, "rtsp://") {
        if input.secret_ref.is_some() {
            errors.push(format!(
                "{path}.secretRef is only for SRT passphrases; use {path}.rtsp.passwordRef"
            ));
        }
        if !input.srt.is_default_contract() {
            errors.push(format!(
                "{path}.srt contains non-default values for an rtsp:// input"
            ));
        }
        validate_rtsp_uri(&input.uri, &format!("{path}.uri"), errors);
        match input.rtsp.as_ref() {
            Some(config) => validate_rtsp(config, &format!("{path}.rtsp"), errors),
            None => errors.push(format!(
                "{path}.rtsp is required for an rtsp:// input so transport and timeout intent are explicit"
            )),
        }
        return;
    }

    errors.push(format!(
        "{path}.uri must use a currently declared input scheme: srt:// or rtsp://"
    ));
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigHeader {
    api_version: String,
    kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaJob {
    pub api_version: String,
    pub kind: String,
    pub metadata: Metadata,
    pub inputs: Vec<InputConfig>,
    #[serde(default)]
    pub processing: ProcessingConfig,
    pub outputs: Vec<JobOutputConfig>,
    #[serde(default)]
    pub taps: Vec<TapConfig>,
    #[serde(default)]
    pub failure_policy: FailurePolicyConfig,
    #[serde(default)]
    pub control: ControlConfig,
}

impl MediaJob {
    fn into_pipeline_config(self) -> Result<PipelineConfig, ConfigError> {
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
        if self.outputs.len() != 1 {
            errors.push(format!(
                "exactly one output is supported in v0.3, got {}; multi-output is scheduled for v0.4",
                self.outputs.len()
            ));
        }

        let mut output_names = HashSet::new();
        for (index, output) in self.outputs.iter().enumerate() {
            if output.name.trim().is_empty() {
                errors.push(format!("outputs[{index}].name must not be empty"));
            } else if !output_names.insert(output.name.as_str()) {
                errors.push(format!(
                    "outputs[{index}].name {:?} is duplicated",
                    output.name
                ));
            }
        }

        if self.taps.len() > 1 {
            errors.push(
                "at most one directorSignals tap is supported in v0.3; general analyzer taps are scheduled for v0.4"
                    .to_owned(),
            );
        }
        let mut tap_names = HashSet::new();
        for (index, tap) in self.taps.iter().enumerate() {
            if tap.name.trim().is_empty() {
                errors.push(format!("taps[{index}].name must not be empty"));
            } else if !tap_names.insert(tap.name.as_str()) {
                errors.push(format!("taps[{index}].name {:?} is duplicated", tap.name));
            }
        }

        if !errors.is_empty() {
            return Err(ConfigError::Validation(errors));
        }

        let output = self
            .outputs
            .into_iter()
            .next()
            .expect("output count was validated");
        let tap = self.taps.into_iter().next();
        let (fast_analyzers, vlm_advisor) = tap.map_or_else(
            || (FastAnalyzersConfig::disabled(), VlmAdvisorConfig::default()),
            |tap| (tap.fast_analyzers, tap.vlm_advisor),
        );

        Ok(PipelineConfig {
            api_version: API_VERSION.to_owned(),
            kind: KIND.to_owned(),
            metadata: self.metadata,
            inputs: self.inputs,
            output: output.into(),
            media: MediaConfig {
                video: self.processing.video,
                audio: self.processing.audio,
            },
            sync: self.processing.timing,
            fast_analyzers,
            vlm_advisor,
            director_policy: self.processing.switching.policy,
            audio_switch: self.processing.switching.audio,
            failure_policy: self.failure_policy,
            control: self.control,
        })
    }

    fn from_pipeline_config(config: PipelineConfig) -> Self {
        Self {
            api_version: API_VERSION.to_owned(),
            kind: KIND.to_owned(),
            metadata: config.metadata,
            inputs: config.inputs,
            processing: ProcessingConfig {
                video: config.media.video,
                audio: config.media.audio,
                timing: config.sync,
                switching: SwitchingConfig {
                    policy: config.director_policy,
                    audio: config.audio_switch,
                },
            },
            outputs: vec![JobOutputConfig {
                name: "program".to_owned(),
                uri: config.output.uri,
                secret_ref: config.output.secret_ref,
                srt: config.output.srt,
            }],
            taps: vec![TapConfig {
                name: "director-signals".to_owned(),
                kind: TapKind::DirectorSignals,
                fast_analyzers: config.fast_analyzers,
                vlm_advisor: config.vlm_advisor,
            }],
            failure_policy: config.failure_policy,
            control: config.control,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProcessingConfig {
    #[serde(default)]
    pub video: VideoConfig,
    #[serde(default)]
    pub audio: AudioConfig,
    #[serde(default)]
    pub timing: SyncConfig,
    #[serde(default)]
    pub switching: SwitchingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SwitchingConfig {
    #[serde(default)]
    pub policy: DirectorPolicyConfig,
    #[serde(default)]
    pub audio: AudioSwitchConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JobOutputConfig {
    pub name: String,
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<SecretRef>,
    #[serde(default)]
    pub srt: SrtConfig,
}

impl From<JobOutputConfig> for OutputConfig {
    fn from(output: JobOutputConfig) -> Self {
        Self {
            uri: output.uri,
            secret_ref: output.secret_ref,
            srt: output.srt,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TapConfig {
    pub name: String,
    pub kind: TapKind,
    #[serde(default)]
    pub fast_analyzers: FastAnalyzersConfig,
    #[serde(default)]
    pub vlm_advisor: VlmAdvisorConfig,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TapKind {
    DirectorSignals,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacyPipelineConfig {
    api_version: String,
    kind: String,
    metadata: Metadata,
    inputs: Vec<InputConfig>,
    output: OutputConfig,
    #[serde(default)]
    media: MediaConfig,
    #[serde(default)]
    sync: SyncConfig,
    #[serde(default)]
    fast_analyzers: FastAnalyzersConfig,
    #[serde(default)]
    vlm_advisor: VlmAdvisorConfig,
    #[serde(default)]
    director_policy: DirectorPolicyConfig,
    #[serde(default)]
    audio_switch: AudioSwitchConfig,
    #[serde(default)]
    failure_policy: FailurePolicyConfig,
    #[serde(default)]
    control: ControlConfig,
}

impl LegacyPipelineConfig {
    fn into_pipeline_config(self) -> Result<PipelineConfig, ConfigError> {
        let mut errors = Vec::new();
        if self.api_version != LEGACY_API_VERSION {
            errors.push(format!(
                "legacy apiVersion must be {LEGACY_API_VERSION:?}, got {:?}",
                self.api_version
            ));
        }
        if self.kind != LEGACY_KIND {
            errors.push(format!(
                "legacy kind must be {LEGACY_KIND:?}, got {:?}",
                self.kind
            ));
        }
        if !errors.is_empty() {
            return Err(ConfigError::Validation(errors));
        }

        let config = PipelineConfig {
            api_version: API_VERSION.to_owned(),
            kind: KIND.to_owned(),
            metadata: self.metadata,
            inputs: self.inputs,
            output: self.output,
            media: self.media,
            sync: self.sync,
            fast_analyzers: self.fast_analyzers,
            vlm_advisor: self.vlm_advisor,
            director_policy: self.director_policy,
            audio_switch: self.audio_switch,
            failure_policy: self.failure_policy,
            control: self.control,
        };
        config.validate()?;
        Ok(config)
    }
}

pub fn convert_legacy_yaml(contents: &str) -> Result<String, ConfigError> {
    let legacy: LegacyPipelineConfig = serde_yaml::from_str(contents)?;
    let config = legacy.into_pipeline_config()?;
    let job = MediaJob::from_pipeline_config(config);
    Ok(serde_yaml::to_string(&job)?)
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
    validate_reconnect(&config.reconnect, &format!("{path}.reconnect"), errors);
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

fn validate_rtsp(config: &RtspConfig, path: &str, errors: &mut Vec<String>) {
    if !(100..=60_000).contains(&config.connect_timeout_ms) {
        errors.push(format!(
            "{path}.connectTimeoutMs must be between 100 and 60000"
        ));
    }
    if !(500..=120_000).contains(&config.read_timeout_ms) {
        errors.push(format!(
            "{path}.readTimeoutMs must be between 500 and 120000"
        ));
    }
    if !(1_000..=300_000).contains(&config.keepalive_ms) {
        errors.push(format!(
            "{path}.keepaliveMs must be between 1000 and 300000"
        ));
    }
    validate_reconnect(&config.reconnect, &format!("{path}.reconnect"), errors);
    validate_secret_ref(
        config.password_ref.as_ref(),
        &format!("{path}.passwordRef"),
        errors,
    );

    let username_present = config
        .username
        .as_deref()
        .is_some_and(|username| !username.is_empty());
    let password_present = config.password_ref.is_some();
    if username_present != password_present {
        errors.push(format!(
            "{path}.username and {path}.passwordRef must be set together"
        ));
    }
    if config.username.as_deref().is_some_and(|username| {
        username.is_empty() || username.len() > 256 || username.contains(['\r', '\n'])
    }) {
        errors.push(format!(
            "{path}.username must be non-empty, at most 256 characters, and contain no line breaks"
        ));
    }
}

fn validate_reconnect(config: &ReconnectConfig, path: &str, errors: &mut Vec<String>) {
    if config.initial_backoff_ms == 0
        || config.initial_backoff_ms > config.max_backoff_ms
        || config.max_backoff_ms > 60_000
    {
        errors.push(format!(
            "{path} backoff must satisfy 1 <= initialBackoffMs <= maxBackoffMs <= 60000"
        ));
    }
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
    if !has_scheme(uri, "srt://") {
        errors.push(format!("{path} must use the srt:// scheme"));
    }
    let authority = uri
        .get("srt://".len()..)
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

fn validate_rtsp_uri(uri: &str, path: &str, errors: &mut Vec<String>) {
    if !has_scheme(uri, "rtsp://") {
        errors.push(format!("{path} must use the rtsp:// scheme"));
    }
    let authority = uri
        .get("rtsp://".len()..)
        .and_then(|rest| rest.split(['/', '?']).next())
        .unwrap_or_default();
    if authority.is_empty() {
        errors.push(format!("{path} must include a camera host"));
    }
    if authority.contains('@') {
        errors.push(format!(
            "{path} contains URI userinfo; use rtsp.username and rtsp.passwordRef"
        ));
    }
    let lower = uri.to_ascii_lowercase();
    for sensitive in ["password=", "passphrase=", "token=", "secret=", "auth="] {
        if lower.contains(sensitive) {
            errors.push(format!(
                "{path} contains inline credentials; use rtsp.passwordRef"
            ));
            break;
        }
    }
}

fn has_scheme(uri: &str, scheme: &str) -> bool {
    uri.get(..scheme.len())
        .is_some_and(|actual| actual.eq_ignore_ascii_case(scheme))
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
        errors.push(format!("{path} must set exactly one of env or file"));
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<SecretRef>,
    #[serde(default)]
    pub srt: SrtConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtsp: Option<RtspConfig>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<SecretRef>,
    #[serde(default)]
    pub srt: SrtConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SecretRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<SrtMode>,
    #[serde(default = "default_srt_latency")]
    pub latency_ms: u64,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_ms: u64,
    #[serde(default)]
    pub reconnect: ReconnectConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
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

    fn is_default_contract(&self) -> bool {
        self.mode.is_none()
            && self.latency_ms == default_srt_latency()
            && self.connect_timeout_ms == default_connect_timeout()
            && self.reconnect.enabled
            && self.reconnect.initial_backoff_ms == default_initial_backoff()
            && self.reconnect.max_backoff_ms == default_max_backoff()
            && self.stream_id.is_none()
            && self.stream_id_ref.is_none()
            && self.key_length == default_srt_key_length()
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
pub struct RtspConfig {
    #[serde(default)]
    pub transport: RtspTransport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_ref: Option<SecretRef>,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_ms: u64,
    #[serde(default = "default_rtsp_read_timeout")]
    pub read_timeout_ms: u64,
    #[serde(default = "default_rtsp_keepalive")]
    pub keepalive_ms: u64,
    #[serde(default)]
    pub reconnect: ReconnectConfig,
}

impl Default for RtspConfig {
    fn default() -> Self {
        Self {
            transport: RtspTransport::default(),
            username: None,
            password_ref: None,
            connect_timeout_ms: default_connect_timeout(),
            read_timeout_ms: default_rtsp_read_timeout(),
            keepalive_ms: default_rtsp_keepalive(),
            reconnect: ReconnectConfig::default(),
        }
    }
}

impl RtspConfig {
    pub fn resolve_password(&self) -> Result<Option<String>, SecretError> {
        self.password_ref
            .as_ref()
            .map(SecretRef::resolve)
            .transpose()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RtspTransport {
    #[default]
    Tcp,
    Udp,
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
const fn default_rtsp_read_timeout() -> u64 {
    5_000
}
const fn default_rtsp_keepalive() -> u64 {
    15_000
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

impl FastAnalyzersConfig {
    fn disabled() -> Self {
        Self {
            vad: false,
            person: false,
            mouth_motion: false,
            quality: false,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
    use super::{
        ConfigError, PipelineConfig, RtspTransport, convert_legacy_yaml, validate_srt_uri,
    };

    #[test]
    fn accepts_the_reference_alpha_configuration() {
        let yaml = include_str!("../../../examples/director.yaml");
        let config = PipelineConfig::from_yaml(yaml).expect("reference configuration is valid");
        assert_eq!(config.inputs.len(), 2);
        assert_eq!(config.sync.max_skew_ms, 80);
        assert_eq!(config.media.video.gop_ms, 1_000);
        assert!(config.fast_analyzers.vad);
    }

    #[test]
    fn accepts_one_input_and_rejects_a_missing_master() {
        let yaml = include_str!("../../../examples/single-srt.yaml");
        let config = PipelineConfig::from_yaml(yaml).expect("single-input configuration is valid");
        assert_eq!(config.inputs.len(), 1);
        assert!(!config.fast_analyzers.vad);

        let invalid = yaml.replace("masterInput: 0", "masterInput: 1");
        let error = PipelineConfig::from_yaml(&invalid).expect_err("missing master is rejected");
        assert!(error.to_string().contains("processing.timing.masterInput"));
    }

    #[test]
    fn legacy_configuration_requires_explicit_conversion() {
        let legacy = include_str!("../../../examples/v1alpha1.yaml");
        assert!(matches!(
            PipelineConfig::from_yaml(legacy),
            Err(ConfigError::LegacyRequiresConversion)
        ));

        let converted = convert_legacy_yaml(legacy).expect("legacy configuration converts");
        assert!(converted.contains("apiVersion: aimedia/v1alpha2"));
        assert!(converted.contains("kind: MediaJob"));
        assert!(converted.contains("outputs:"));
        let config = PipelineConfig::from_yaml(&converted).expect("converted MediaJob is valid");
        assert_eq!(config.metadata.name, "legacy-single-srt");
        assert_eq!(config.inputs[0].name, "program");
        assert_eq!(config.output.uri, "srt://127.0.0.1:10000");
    }

    #[test]
    fn rejects_multiple_outputs_until_fanout_is_implemented() {
        let yaml = include_str!("../../../examples/single-srt.yaml");
        let invalid = yaml.replace(
            "taps: []",
            "  - name: backup\n    uri: srt://127.0.0.1:10001\n\ntaps: []",
        );
        let error = PipelineConfig::from_yaml(&invalid).expect_err("fan-out must be rejected");
        assert!(
            error
                .to_string()
                .contains("multi-output is scheduled for v0.4")
        );
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

    #[test]
    fn accepts_explicit_rtsp_contract_without_treating_it_as_srt() {
        let yaml = include_str!("../../../examples/rtsp.yaml");
        let config = PipelineConfig::from_yaml(yaml).expect("RTSP contract should be valid");
        let rtsp = config.inputs[0]
            .rtsp
            .as_ref()
            .expect("RTSP input keeps protocol-specific settings");
        assert_eq!(rtsp.transport, RtspTransport::Tcp);
        assert_eq!(rtsp.read_timeout_ms, 5_000);
        assert_eq!(
            rtsp.password_ref
                .as_ref()
                .and_then(|reference| reference.env.as_deref()),
            Some("AIMEDIA_CAMERA_PASSWORD")
        );
    }

    #[test]
    fn rejects_inline_rtsp_credentials_and_cross_protocol_settings() {
        let yaml = include_str!("../../../examples/rtsp.yaml");
        let inline = yaml.replace(
            "rtsp://192.0.2.10/Streaming/Channels/101",
            "rtsp://admin:secret@192.0.2.10/Streaming/Channels/101?token=secret",
        );
        let error = PipelineConfig::from_yaml(&inline).expect_err("URI credentials are rejected");
        let message = error.to_string();
        assert!(message.contains("URI userinfo"));
        assert!(message.contains("inline credentials"));

        let cross_protocol =
            yaml.replace("    rtsp:\n", "    srt:\n      latencyMs: 240\n    rtsp:\n");
        let error = PipelineConfig::from_yaml(&cross_protocol)
            .expect_err("non-default SRT settings cannot leak into RTSP");
        assert!(error.to_string().contains("non-default values"));
    }
}
