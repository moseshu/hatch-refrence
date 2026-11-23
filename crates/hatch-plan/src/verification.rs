//! A completion contract is frozen before execution, never learned from its output.
use crate::{Plan, PlanStep};
use hatch_core::{Error, Result};
use hatch_tools::verification::{EvidenceCheck, Verdict};
use serde::{Deserialize, Serialize};

pub use crate::verification_store::{Executed, ExecutionRecord, LeaseFence, VerificationStore};
pub use hatch_tools::verification::{
    EffectState, EvidenceCheck as Check, Layer, Verdict as CheckVerdict,
};
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
    /// The gate verdict: aggregated over enforced checks only. This is
    /// what `accepted()` judges; Observe-layer checks never move it.
    pub verdict: Verdict,
    /// The shadow verdict: aggregated over every check, i.e. what the
    /// gate would have decided if all layers were enforced. `None` on
    /// reports persisted before layers existed (shadow == gate then).
    #[serde(default)]
    pub shadow_verdict: Option<Verdict>,
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
    /// What the gate would have decided with every layer enforced.
    pub fn shadow(&self) -> Verdict {
        self.shadow_verdict.clone().unwrap_or_else(|| self.verdict.clone())
    }
    pub fn reason(&self) -> String {
        if self.mode == VerificationMode::Enforce && !self.is_fresh() {
            return "验证证据已过期且读取预算不足，需要重新核对或新计划版本".into();
        }
        self.checks
            .iter()
            .filter(|c| c.enforced && matches!(c.verdict, Verdict::Fail | Verdict::Ambiguous))
            .map(|c| format!("{}: {}", c.rule, c.reason))
            .collect::<Vec<_>>()
            .join("；")
    }
    /// Failures among Observe-layer checks: recorded for evaluation,
    /// never a reason the gate blocked.
    pub fn shadow_reason(&self) -> String {
        self.checks
            .iter()
            .filter(|c| !c.enforced && matches!(c.verdict, Verdict::Fail | Verdict::Ambiguous))
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

pub fn verdict(checks: &[EvidenceCheck]) -> Verdict {
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

/// The gate verdict: only enforced checks count. Observe-layer checks
/// contribute to the shadow verdict, never to the gate — putting a
/// new layer in Observe must not relax (or tighten) what the existing
/// layers enforce. With every check enforced this is exactly
/// [`verdict`]; with no enforced checks at all there is nothing to
/// gate on, so the gate is NotApplicable rather than Ambiguous.
pub fn gate_verdict(checks: &[EvidenceCheck]) -> Verdict {
    let enforced: Vec<EvidenceCheck> = checks.iter().filter(|c| c.enforced).cloned().collect();
    if enforced.is_empty() && !checks.is_empty() {
        return Verdict::NotApplicable;
    }
    verdict(&enforced)
}

#[cfg(test)]
mod gate_tests {
    use super::*;
    use hatch_tools::verification::Layer;

    fn check(verdict: Verdict, layer: Layer, enforced: bool) -> EvidenceCheck {
        EvidenceCheck {
            verdict,
            ..EvidenceCheck::unknown("test", "t")
        }
        .with_layer(layer, enforced)
    }

    #[test]
    fn gate_equals_verdict_when_everything_enforced() {
        let checks = vec![
            check(Verdict::Pass, Layer::L1, true),
            check(Verdict::Fail, Layer::L1, true),
        ];
        assert_eq!(gate_verdict(&checks), verdict(&checks));
        assert_eq!(gate_verdict(&checks), Verdict::Fail);
        assert_eq!(gate_verdict(&[]), Verdict::Ambiguous);
    }

    #[test]
    fn observe_failure_moves_shadow_not_gate() {
        let checks = vec![
            check(Verdict::Pass, Layer::L1, true),
            check(Verdict::Fail, Layer::L2, false),
        ];
        assert_eq!(gate_verdict(&checks), Verdict::Pass);
        assert_eq!(verdict(&checks), Verdict::Fail);
    }

    #[test]
    fn observe_ambiguous_does_not_block_gate() {
        let checks = vec![
            check(Verdict::Pass, Layer::L1, true),
            check(Verdict::Ambiguous, Layer::L2, false),
        ];
        assert_eq!(gate_verdict(&checks), Verdict::Pass);
        assert_eq!(verdict(&checks), Verdict::Ambiguous);
    }

    #[test]
    fn enforced_failure_still_blocks_with_observe_present() {
        let checks = vec![
            check(Verdict::Fail, Layer::L1, true),
            check(Verdict::Pass, Layer::L2, false),
        ];
        assert_eq!(gate_verdict(&checks), Verdict::Fail);
    }

    #[test]
    fn no_enforced_checks_is_not_applicable_not_ambiguous() {
        let checks = vec![check(Verdict::Fail, Layer::L2, false)];
        assert_eq!(gate_verdict(&checks), Verdict::NotApplicable);
    }

    #[test]
    fn old_report_json_without_layer_fields_still_loads() {
        // A report persisted before layers existed: no layer/enforced
        // on checks, no shadow_verdict on the report.
        let old = r#"{"owner":"p","version":1,"step_id":"s1","contract_digest":"c","rules_digest":"r","mode":"enforce","attempt":1,"observed_at":"2026-11-22T00:00:00Z","verdict":"pass","checks":[{"rule":"file-content-v1","verdict":"pass","effect":"confirmed_applied","source":"workspace_read","resource":"a.txt","expected_digest":"e","observed_digest":"o","reason":"ok"}]}"#;
        let report: VerificationReport = serde_json::from_str(old).unwrap();
        assert_eq!(report.checks[0].layer, Layer::L1);
        assert!(report.checks[0].enforced);
        assert_eq!(report.shadow(), Verdict::Pass);
        assert_eq!(report.shadow_reason(), "");
    }
}
