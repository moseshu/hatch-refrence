//! Typed plans: parsing, validation and deviation detection.
//!
//! A plan is a JSON object the model returns:
//!
//! ```json
//! {"steps": [
//!   {"id": "s1", "title": "…", "tool": "memory_search",
//!    "arguments": {"query": "…"}, "files": [],
//!    "expected": "…", "risk": "low"}
//! ]}
//! ```
//!
//! Validation lives in [`crate::validate`]: it rejects malformed JSON,
//! unknown tools, duplicate step ids and any file path that escapes the
//! workspace. The `files` list is declarative — the runner also compares
//! the full `arguments` object — but paths named in a step are checked up
//! front so a bad plan fails before it is ever approved.

use hatch_core::{ids, Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_STEPS: usize = 32;

/// How risky a step is, as judged by the model at plan time. Advisory only:
/// it does not change the gate — every step still goes through dispatch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Low,
    #[default]
    Medium,
    High,
}

impl RiskLevel {
    pub fn parse(s: &str) -> Result<RiskLevel> {
        match s {
            "low" => Ok(RiskLevel::Low),
            "medium" => Ok(RiskLevel::Medium),
            "high" => Ok(RiskLevel::High),
            other => Err(Error::Invalid(format!(
                "unknown risk level `{other}` (low / medium / high)"
            ))),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            RiskLevel::Low => "low",
            RiskLevel::Medium => "medium",
            RiskLevel::High => "high",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Draft,
    PendingApproval,
    Approved,
    Executing,
    Completed,
    Rejected,
    Superseded,
    Cancelled,
    /// A run stopped because the plan no longer matches what was approved.
    NeedsReapproval,
}

impl PlanStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            PlanStatus::Draft => "draft",
            PlanStatus::PendingApproval => "pending_approval",
            PlanStatus::Approved => "approved",
            PlanStatus::Executing => "executing",
            PlanStatus::Completed => "completed",
            PlanStatus::Rejected => "rejected",
            PlanStatus::Superseded => "superseded",
            PlanStatus::Cancelled => "cancelled",
            PlanStatus::NeedsReapproval => "needs_reapproval",
        }
    }

    pub fn parse(s: &str) -> Result<PlanStatus> {
        [
            PlanStatus::Draft,
            PlanStatus::PendingApproval,
            PlanStatus::Approved,
            PlanStatus::Executing,
            PlanStatus::Completed,
            PlanStatus::Rejected,
            PlanStatus::Superseded,
            PlanStatus::Cancelled,
            PlanStatus::NeedsReapproval,
        ]
        .into_iter()
        .find(|x| x.as_str() == s)
        .ok_or_else(|| Error::Invalid(format!("unknown plan status `{s}`")))
    }

    /// Terminal: no further transition is expected.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            PlanStatus::Completed
                | PlanStatus::Rejected
                | PlanStatus::Superseded
                | PlanStatus::Cancelled
        )
    }
}

/// Per-step execution state (stored in `plan_steps`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    #[default]
    Pending,
    Done,
    Failed,
    /// Dispatched but queued for a gate approval; resume picks it up.
    AwaitingApproval,
}

impl StepStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            StepStatus::Pending => "pending",
            StepStatus::Done => "done",
            StepStatus::Failed => "failed",
            StepStatus::AwaitingApproval => "awaiting_approval",
        }
    }

    pub fn parse(s: &str) -> Result<StepStatus> {
        match s {
            "pending" => Ok(StepStatus::Pending),
            "done" => Ok(StepStatus::Done),
            "failed" => Ok(StepStatus::Failed),
            "awaiting_approval" => Ok(StepStatus::AwaitingApproval),
            other => Err(Error::Invalid(format!("unknown step status `{other}`"))),
        }
    }
}

/// One ordered step of a plan.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanStep {
    pub id: String,
    pub title: String,
    pub tool: String,
    #[serde(default)]
    pub arguments: Value,
    #[serde(default)]
    pub files: Vec<String>,
    pub expected: String,
    #[serde(default)]
    pub risk: RiskLevel,
    /// Execution state; not part of the approved snapshot comparison for
    /// `tool` / `arguments` / `files` (see [`check_step`]).
    #[serde(default)]
    pub status: StepStatus,
    #[serde(default)]
    pub output: String,
    /// Approval id when the step is waiting on the gate queue.
    #[serde(default)]
    pub approval_id: Option<String>,
}

impl PlanStep {
    /// Canonical JSON of exactly what the user approved for this step.
    fn approved_json(&self) -> String {
        serde_json::json!({
            "id": self.id,
            "title": self.title,
            "tool": self.tool,
            "arguments": self.arguments,
            "files": self.files,
            "expected": self.expected,
            "risk": self.risk.as_str(),
        })
        .to_string()
    }
}

/// What changed between the approved plan and the plan about to run.
#[derive(Clone, Debug, PartialEq)]
pub enum Deviation {
    /// The plan now has a step the approval never covered.
    ExtraStep { step_id: String },
    /// An approved step is missing from the plan.
    MissingStep { step_id: String },
    /// A step's tool, arguments or files differ from the approved version.
    ChangedStep { step_id: String, detail: String },
}

impl Deviation {
    pub fn describe(&self) -> String {
        match self {
            Deviation::ExtraStep { step_id } => format!("计划中多出未批准的步骤 {step_id}"),
            Deviation::MissingStep { step_id } => {
                format!("批准的步骤 {step_id} 不见了（被跳过或删除）")
            }
            Deviation::ChangedStep { step_id, detail } => {
                format!("步骤 {step_id} 与批准时不一致：{detail}")
            }
        }
    }
}

/// A whole plan: the exact task it was made for, its steps, its state and
/// the audit trail of the approval decision.
#[derive(Clone, Debug)]
pub struct Plan {
    pub id: String,
    /// The exact prompt / task this plan was made for.
    pub task: String,
    pub status: PlanStatus,
    /// Model that produced the plan (`""` when hand-written / edited).
    pub model: String,
    pub steps: Vec<PlanStep>,
    pub created_at: String,
    pub updated_at: String,
    pub decided_at: Option<String>,
    pub decided_by: String,
    pub decision_note: String,
    /// Canonical JSON of the approved steps; empty until first approval.
    pub approved_steps_json: String,
    pub approved_hash: String,
    /// Why the plan needs re-approval (deviation description).
    pub deviation: String,
}

impl Plan {
    /// Hash of the canonical approved-step JSON (what the runner checks).
    pub fn steps_hash(steps: &[PlanStep]) -> String {
        let canonical: Vec<String> = steps.iter().map(PlanStep::approved_json).collect();
        ids::digest(&[&canonical.join("\n")])
    }

    pub fn steps_json(steps: &[PlanStep]) -> String {
        let canonical: Vec<String> = steps.iter().map(PlanStep::approved_json).collect();
        serde_json::to_string(&canonical).unwrap_or_default()
    }
}

/// Outcome of one executed step, for the run report.
#[derive(Clone, Debug)]
pub struct StepOutcome {
    pub step_id: String,
    pub status: StepStatus,
    pub output: String,
    pub approval_id: Option<String>,
}

/// A plan header without steps (for `list`).
#[derive(Clone, Debug)]
pub struct PlanSummary {
    pub id: String,
    pub task: String,
    pub status: PlanStatus,
    pub created_at: String,
    pub updated_at: String,
    pub steps: usize,
}

/// Edits allowed on one step (`hatch plan edit`). `None` = leave unchanged.
#[derive(Clone, Debug, Default)]
pub struct StepEdit {
    pub tool: Option<String>,
    pub arguments: Option<serde_json::Value>,
    pub files: Option<Vec<String>>,
    pub title: Option<String>,
    pub expected: Option<String>,
    pub risk: Option<RiskLevel>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_roundtrip_and_terminal() {
        for s in [
            PlanStatus::Draft,
            PlanStatus::NeedsReapproval,
            PlanStatus::Completed,
        ] {
            assert_eq!(PlanStatus::parse(s.as_str()).unwrap(), s);
        }
        assert!(PlanStatus::Completed.is_terminal());
        assert!(!PlanStatus::Approved.is_terminal());
        assert!(PlanStatus::parse("bogus").is_err());
    }
}
