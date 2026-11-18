//! Read-only evidence checks against the immutable approved payload.
//! A transport receipt locates a resource; it is never sufficient evidence.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Fail,
    Ambiguous,
    NotApplicable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectState {
    ConfirmedApplied,
    ConfirmedNotApplied,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvidenceCheck {
    pub rule: String,
    pub verdict: Verdict,
    pub effect: EffectState,
    pub source: String,
    pub resource: String,
    pub expected_digest: String,
    pub observed_digest: String,
    pub reason: String,
}

impl EvidenceCheck {
    pub fn unknown(rule: &str, reason: impl Into<String>) -> Self {
        Self {
            rule: rule.into(),
            verdict: Verdict::Ambiguous,
            effect: EffectState::Unknown,
            source: String::new(),
            resource: String::new(),
            expected_digest: String::new(),
            observed_digest: String::new(),
            reason: reason.into(),
        }
    }
    pub fn compare(
        rule: &str,
        source: &str,
        resource: &str,
        expected: &[u8],
        observed: &[u8],
    ) -> Self {
        let matches = expected == observed;
        Self {
            rule: rule.into(),
            verdict: if matches {
                Verdict::Pass
            } else {
                Verdict::Fail
            },
            effect: EffectState::ConfirmedApplied,
            source: source.into(),
            resource: resource.into(),
            expected_digest: crate::checkpoint::content_hash(expected),
            observed_digest: crate::checkpoint::content_hash(observed),
            reason: if matches {
                "独立回读与批准内容一致"
            } else {
                "资源存在，但独立回读与批准内容不符；不能重复原动作"
            }
            .into(),
        }
    }
}

/// Whether an unapproved direct tool call needs an independent completion check.
pub fn requires_evidence(class: hatch_policy::ActionClass) -> bool {
    use hatch_policy::ActionClass::*;
    !matches!(
        class,
        ReadPersonal | FetchWeb | BrowseLocal | DraftMessage | WriteMemory
    )
}
