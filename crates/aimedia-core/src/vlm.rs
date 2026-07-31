use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

use crate::director::CameraSnapshot;

const SYSTEM_PROMPT: &str = "\
You are a conservative two-camera live director. Return only JSON matching the supplied schema. \
Prefer the active speaker and a healthy, well-composed shot. Avoid unnecessary cuts. Your advice \
is advisory and expires quickly.";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VlmAdvice {
    pub observed_at_ms: u64,
    pub expires_at_ms: u64,
    pub scores: [f32; 2],
    pub recommended_input: usize,
    pub reason: String,
}

impl VlmAdvice {
    #[must_use]
    pub const fn valid_at(&self, now_ms: u64) -> bool {
        now_ms >= self.observed_at_ms && now_ms <= self.expires_at_ms
    }
}

#[derive(Debug, Clone)]
pub struct AdvisorRequest {
    pub observed_at_ms: u64,
    pub active_input: usize,
    pub cameras: [CameraSnapshot; 2],
    /// Images must be explicit data URLs or HTTPS URLs approved by the caller.
    pub image_urls: [String; 2],
    pub valid_for_ms: u64,
}

#[derive(Debug, Error)]
pub enum AdvisorError {
    #[error("VLM request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("VLM response did not include string content")]
    MissingContent,
    #[error("VLM returned invalid JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("VLM returned invalid advice: {0}")]
    InvalidAdvice(String),
}

#[async_trait]
pub trait DirectorAdvisor: Send + Sync {
    async fn advise(&self, request: &AdvisorRequest) -> Result<VlmAdvice, AdvisorError>;
}

#[derive(Debug, Clone)]
pub struct OpenAiCompatibleAdvisor {
    client: Client,
    endpoint: String,
    model: String,
    bearer_token: Option<String>,
}

impl OpenAiCompatibleAdvisor {
    pub fn new(
        endpoint: impl Into<String>,
        model: impl Into<String>,
        bearer_token: Option<String>,
        deadline: Duration,
    ) -> Result<Self, AdvisorError> {
        let client = Client::builder().timeout(deadline).build()?;
        Ok(Self {
            client,
            endpoint: endpoint.into(),
            model: model.into(),
            bearer_token,
        })
    }

    fn payload(&self, request: &AdvisorRequest) -> Value {
        let metrics = json!({
            "activeInput": request.active_input,
            "cameras": &request.cameras,
            "instruction": "Score both cameras from 0 to 1 and recommend input 0 or 1."
        });
        json!({
            "model": self.model,
            "temperature": 0,
            "messages": [
                {
                    "role": "system",
                    "content": SYSTEM_PROMPT
                },
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "text",
                            "text": metrics.to_string()
                        },
                        {
                            "type": "image_url",
                            "image_url": { "url": request.image_urls[0].as_str() }
                        },
                        {
                            "type": "image_url",
                            "image_url": { "url": request.image_urls[1].as_str() }
                        }
                    ]
                }
            ],
            "response_format": {
                "type": "json_schema",
                "json_schema": {
                    "name": "director_advice",
                    "strict": true,
                    "schema": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "scores": {
                                "type": "array",
                                "items": { "type": "number", "minimum": 0, "maximum": 1 },
                                "minItems": 2,
                                "maxItems": 2
                            },
                            "recommendedInput": {
                                "type": "integer",
                                "minimum": 0,
                                "maximum": 1
                            },
                            "reason": {
                                "type": "string",
                                "maxLength": 240
                            }
                        },
                        "required": ["scores", "recommendedInput", "reason"]
                    }
                }
            }
        })
    }
}

#[async_trait]
impl DirectorAdvisor for OpenAiCompatibleAdvisor {
    async fn advise(&self, request: &AdvisorRequest) -> Result<VlmAdvice, AdvisorError> {
        let mut builder = self
            .client
            .post(&self.endpoint)
            .json(&self.payload(request));
        if let Some(token) = &self.bearer_token {
            builder = builder.bearer_auth(token);
        }
        let response: ChatResponse = builder.send().await?.error_for_status()?.json().await?;
        let content = response
            .choices
            .first()
            .and_then(|choice| choice.message.content.as_deref())
            .ok_or(AdvisorError::MissingContent)?;
        let payload: AdvicePayload = serde_json::from_str(content)?;
        validate_payload(&payload)?;
        let valid_for_ms = request.valid_for_ms.min(3_000);
        Ok(VlmAdvice {
            observed_at_ms: request.observed_at_ms,
            expires_at_ms: request.observed_at_ms.saturating_add(valid_for_ms),
            scores: payload.scores,
            recommended_input: payload.recommended_input,
            reason: payload.reason,
        })
    }
}

fn validate_payload(payload: &AdvicePayload) -> Result<(), AdvisorError> {
    if payload.recommended_input > 1 {
        return Err(AdvisorError::InvalidAdvice(
            "recommendedInput must be 0 or 1".to_owned(),
        ));
    }
    if payload.reason.trim().is_empty() || payload.reason.chars().count() > 240 {
        return Err(AdvisorError::InvalidAdvice(
            "reason must contain 1..=240 characters".to_owned(),
        ));
    }
    if payload
        .scores
        .iter()
        .any(|score| !score.is_finite() || !(0.0..=1.0).contains(score))
    {
        return Err(AdvisorError::InvalidAdvice(
            "scores must be finite values between 0 and 1".to_owned(),
        ));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

#[derive(Debug, Deserialize)]
struct ChatMessage {
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AdvicePayload {
    scores: [f32; 2],
    recommended_input: usize,
    reason: String,
}
