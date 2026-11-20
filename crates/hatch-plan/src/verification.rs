//! A completion contract is frozen before execution, never learned from its output.
use crate::{Plan, PlanStep};
use hatch_core::{Error, Result};
use hatch_tools::verification::{EvidenceCheck, Verdict};
use serde::{Deserialize, Serialize};

pub use crate::verification_store::{Executed, ExecutionRecord, VerificationStore};
pub use hatch_tools::verification::{EffectState, EvidenceCheck as Check, Verdict as CheckVerdict};
pub const RULESET: &str = include_str!("../../../verification/rules/builtin-v1.json");
pub const MAX_OBSERVATIONS: u32 = 3;
pub const MAX_EVIDENCE_AGE_SECS: i64 = 300;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationMode {
    Off,
    Observe,
    Enforce,
}
impl VerificationMode {
    pub fn configured() -> Result<Self> {
        match std::env::var("HATCH_VERIFICATION_MODE").as_deref() {
            Err(std::env::VarError::NotPresent) | Ok("enforce") => Ok(Self::Enforce),
            Ok("observe") => Ok(Self::Observe),
            Ok("off") => Ok(Self::Off),
            _ => Err(Error::Invalid(
                "HATCH_VERIFICATION_MODE 必须是 off、observe 或 enforce".into(),
            )),
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Observe => "observe",
            Self::Enforce => "enforce",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VerificationReport {
    pub owner: String,
    pub version: i64,
    pub step_id: String,
    pub contract_digest: String,
    pub rules_digest: String,
    pub mode: VerificationMode,
    pub attempt: u32,
    pub observed_at: String,
    pub verdict: Verdict,
    pub checks: Vec<EvidenceCheck>,
}
impl VerificationReport {
    pub fn accepted(&self) -> bool {
        self.mode != VerificationMode::Enforce
            || (matches!(self.verdict, Verdict::Pass | Verdict::NotApplicable) && self.is_fresh())
    }
    pub fn is_fresh(&self) -> bool {
        hatch_core::time::parse_rfc3339(&self.observed_at).is_ok_and(|at| {
            let age = hatch_core::time::now()
                .signed_duration_since(at)
                .num_seconds();
            (0..=MAX_EVIDENCE_AGE_SECS).contains(&age)
        })
    }
    pub fn reason(&self) -> String {
        if self.mode == VerificationMode::Enforce && !self.is_fresh() {
            return "验证证据已过期且读取预算不足，需要重新核对或新计划版本".into();
        }
        self.checks
            .iter()
            .filter(|c| matches!(c.verdict, Verdict::Fail | Verdict::Ambiguous))
            .map(|c| format!("{}: {}", c.rule, c.reason))
            .collect::<Vec<_>>()
            .join("；")
    }
}

pub fn digest(bytes: &[u8]) -> String {
    hatch_tools::checkpoint::content_hash(bytes)
}

pub(crate) fn contract(plan: &Plan, step: &PlanStep) -> Result<String> {
    Ok(
        serde_json::json!({"schema":1,"rules":digest(RULESET.as_bytes()),
        "goal":plan.goal,"task":plan.task,"step":step.id,"tool":step.tool,
        "arguments":step.arguments,"files":step.files,"expected":step.expected})
        .to_string(),
    )
}

pub(crate) fn verdict(checks: &[EvidenceCheck]) -> Verdict {
    if checks.iter().any(|c| c.verdict == Verdict::Fail) {
        Verdict::Fail
    } else if checks.is_empty() || checks.iter().any(|c| c.verdict == Verdict::Ambiguous) {
        Verdict::Ambiguous
    } else if checks.iter().all(|c| c.verdict == Verdict::NotApplicable) {
        Verdict::NotApplicable
    } else {
        Verdict::Pass
    }
}
