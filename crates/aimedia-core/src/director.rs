use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{config::DirectorPolicyConfig, vlm::VlmAdvice};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FastSignals {
    pub vad: f32,
    pub mouth_motion: f32,
    pub composition: f32,
    pub quality: f32,
    #[serde(default = "default_health_score")]
    pub transport_health: f32,
}

impl FastSignals {
    #[must_use]
    pub fn score(self) -> f32 {
        let score = 0.30 * clamp_unit(self.vad)
            + 0.25 * clamp_unit(self.mouth_motion)
            + 0.20 * clamp_unit(self.composition)
            + 0.15 * clamp_unit(self.quality)
            + 0.10 * clamp_unit(self.transport_health);
        clamp_unit(score)
    }
}

const fn default_health_score() -> f32 {
    1.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CameraSnapshot {
    pub name: String,
    pub fast: FastSignals,
    #[serde(default = "default_true")]
    pub healthy: bool,
    #[serde(default = "default_true")]
    pub synchronized: bool,
    #[serde(default)]
    pub frozen: bool,
    #[serde(default)]
    pub skew_ms: u64,
}

impl CameraSnapshot {
    #[must_use]
    pub const fn eligible(&self) -> bool {
        self.healthy && self.synchronized && !self.frozen
    }
}

const fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SwitchReason {
    Initial,
    NoChange,
    Manual,
    Failover,
    Automatic,
    BothInputsUnavailable,
    AutoPaused,
    CandidateBuilding,
    MinimumShot,
    Cooldown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectorDecision {
    pub at_ms: u64,
    pub active_input: usize,
    pub active_name: String,
    pub scores: [f32; 2],
    pub switched: bool,
    pub request_idr: bool,
    pub reason: SwitchReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub advisor_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_input: Option<usize>,
    pub candidate_held_ms: u64,
}

pub type DirectorEvent = DirectorDecision;

#[derive(Debug, Error)]
pub enum DirectorError {
    #[error("input index must be 0 or 1, got {0}")]
    InvalidInput(usize),
    #[error("manual hold must be greater than zero")]
    ZeroHold,
}

#[derive(Debug, Clone, Copy)]
struct ManualHold {
    input: usize,
    until_ms: u64,
}

#[derive(Debug, Clone, Copy)]
struct Candidate {
    input: usize,
    since_ms: u64,
}

/// Deterministic two-camera director.
///
/// The director consumes scores; it does not perform inference and therefore cannot be blocked by
/// a slow model. VLM advice is bounded by a configured weight and ignored after its deadline.
#[derive(Debug)]
pub struct Director {
    policy: DirectorPolicyConfig,
    vlm_weight: f32,
    active_input: usize,
    last_switch_ms: u64,
    candidate: Option<Candidate>,
    manual_hold: Option<ManualHold>,
    auto_enabled: bool,
    initialized: bool,
}

impl Director {
    #[must_use]
    pub fn new(
        policy: DirectorPolicyConfig,
        vlm_weight: f32,
        initial_input: usize,
        now_ms: u64,
    ) -> Self {
        Self {
            policy,
            vlm_weight: vlm_weight.clamp(0.0, 0.25),
            active_input: initial_input.min(1),
            last_switch_ms: now_ms,
            candidate: None,
            manual_hold: None,
            auto_enabled: true,
            initialized: false,
        }
    }

    #[must_use]
    pub const fn active_input(&self) -> usize {
        self.active_input
    }

    pub fn take(&mut self, input: usize, hold_ms: u64, now_ms: u64) -> Result<(), DirectorError> {
        if input > 1 {
            return Err(DirectorError::InvalidInput(input));
        }
        if hold_ms == 0 {
            return Err(DirectorError::ZeroHold);
        }
        self.manual_hold = Some(ManualHold {
            input,
            until_ms: now_ms.saturating_add(hold_ms),
        });
        self.candidate = None;
        Ok(())
    }

    pub fn pause_auto(&mut self) {
        self.auto_enabled = false;
        self.candidate = None;
    }

    pub fn resume_auto(&mut self) {
        self.auto_enabled = true;
        self.manual_hold = None;
        self.candidate = None;
    }

    #[must_use]
    pub fn evaluate(
        &mut self,
        now_ms: u64,
        cameras: &[CameraSnapshot; 2],
        advice: Option<&VlmAdvice>,
    ) -> DirectorDecision {
        let (scores, advisor_reason) = self.combined_scores(now_ms, cameras, advice);
        if !self.initialized {
            self.initialized = true;
            if !cameras[self.active_input].eligible() && cameras[1 - self.active_input].eligible() {
                self.active_input = 1 - self.active_input;
            }
            return self.decision(
                now_ms,
                cameras,
                scores,
                false,
                SwitchReason::Initial,
                advisor_reason,
            );
        }

        if let Some(hold) = self.manual_hold {
            if now_ms < hold.until_ms {
                if cameras[hold.input].eligible() {
                    return self.switch_or_hold(
                        hold.input,
                        now_ms,
                        cameras,
                        scores,
                        SwitchReason::Manual,
                        advisor_reason,
                    );
                }
                if cameras[self.active_input].eligible() {
                    return self.decision(
                        now_ms,
                        cameras,
                        scores,
                        false,
                        SwitchReason::AutoPaused,
                        advisor_reason,
                    );
                }
                return self.failover(now_ms, cameras, scores, advisor_reason);
            }
            self.manual_hold = None;
        }

        if !cameras[self.active_input].eligible() {
            return self.failover(now_ms, cameras, scores, advisor_reason);
        }

        if !self.auto_enabled {
            return self.decision(
                now_ms,
                cameras,
                scores,
                false,
                SwitchReason::AutoPaused,
                advisor_reason,
            );
        }

        let candidate_input = 1 - self.active_input;
        if !cameras[candidate_input].eligible()
            || scores[candidate_input] < scores[self.active_input] + self.policy.score_margin
        {
            self.candidate = None;
            return self.decision(
                now_ms,
                cameras,
                scores,
                false,
                SwitchReason::NoChange,
                advisor_reason,
            );
        }

        let active_for_ms = now_ms.saturating_sub(self.last_switch_ms);
        if active_for_ms < self.policy.min_shot_ms {
            self.candidate = None;
            return self.decision(
                now_ms,
                cameras,
                scores,
                false,
                SwitchReason::MinimumShot,
                advisor_reason,
            );
        }
        if active_for_ms < self.policy.cooldown_ms {
            self.candidate = None;
            return self.decision(
                now_ms,
                cameras,
                scores,
                false,
                SwitchReason::Cooldown,
                advisor_reason,
            );
        }

        let candidate = match self.candidate {
            Some(candidate) if candidate.input == candidate_input => candidate,
            _ => {
                let candidate = Candidate {
                    input: candidate_input,
                    since_ms: now_ms,
                };
                self.candidate = Some(candidate);
                candidate
            }
        };
        let held_ms = now_ms.saturating_sub(candidate.since_ms);
        if held_ms < self.policy.candidate_hold_ms {
            return self.decision(
                now_ms,
                cameras,
                scores,
                false,
                SwitchReason::CandidateBuilding,
                advisor_reason,
            );
        }

        self.switch_or_hold(
            candidate_input,
            now_ms,
            cameras,
            scores,
            SwitchReason::Automatic,
            advisor_reason,
        )
    }

    fn failover(
        &mut self,
        now_ms: u64,
        cameras: &[CameraSnapshot; 2],
        scores: [f32; 2],
        advisor_reason: Option<String>,
    ) -> DirectorDecision {
        let alternate = 1 - self.active_input;
        if cameras[alternate].eligible() {
            self.switch_or_hold(
                alternate,
                now_ms,
                cameras,
                scores,
                SwitchReason::Failover,
                advisor_reason,
            )
        } else {
            self.decision(
                now_ms,
                cameras,
                scores,
                false,
                SwitchReason::BothInputsUnavailable,
                advisor_reason,
            )
        }
    }

    fn switch_or_hold(
        &mut self,
        input: usize,
        now_ms: u64,
        cameras: &[CameraSnapshot; 2],
        scores: [f32; 2],
        reason: SwitchReason,
        advisor_reason: Option<String>,
    ) -> DirectorDecision {
        let switched = input != self.active_input;
        if switched {
            self.active_input = input;
            self.last_switch_ms = now_ms;
        }
        self.candidate = None;
        self.decision(
            now_ms,
            cameras,
            scores,
            switched,
            if switched {
                reason
            } else {
                SwitchReason::NoChange
            },
            advisor_reason,
        )
    }

    fn decision(
        &self,
        now_ms: u64,
        cameras: &[CameraSnapshot; 2],
        scores: [f32; 2],
        switched: bool,
        reason: SwitchReason,
        advisor_reason: Option<String>,
    ) -> DirectorDecision {
        let (candidate_input, candidate_held_ms) = self.candidate.map_or((None, 0), |candidate| {
            (
                Some(candidate.input),
                now_ms.saturating_sub(candidate.since_ms),
            )
        });
        DirectorDecision {
            at_ms: now_ms,
            active_input: self.active_input,
            active_name: cameras[self.active_input].name.clone(),
            scores,
            switched,
            request_idr: switched,
            reason,
            advisor_reason,
            candidate_input,
            candidate_held_ms,
        }
    }

    fn combined_scores(
        &self,
        now_ms: u64,
        cameras: &[CameraSnapshot; 2],
        advice: Option<&VlmAdvice>,
    ) -> ([f32; 2], Option<String>) {
        let fast = [cameras[0].fast.score(), cameras[1].fast.score()];
        let Some(advice) = advice.filter(|advice| advice.valid_at(now_ms)) else {
            return (fast, None);
        };
        let weight = self.vlm_weight;
        (
            [
                fast[0] * (1.0 - weight) + clamp_unit(advice.scores[0]) * weight,
                fast[1] * (1.0 - weight) + clamp_unit(advice.scores[1]) * weight,
            ],
            Some(advice.reason.clone()),
        )
    }
}

fn clamp_unit(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}
