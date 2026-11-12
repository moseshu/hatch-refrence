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
    /// Paused while waiting on something external (an approval, the user,
    /// an external dependency). Resumes to `executing` when it arrives —
    /// unlike `needs_reapproval`, which means the content changed and a
    /// fresh approval round is required. Not terminal.
    Blocked,
    Completed,
    Rejected,
    Superseded,
    Cancelled,
    /// A run stopped because the plan no longer matches what was approved.
    NeedsReapproval,
    /// The run died irrecoverably. Terminal. Distinct from a single
    /// failed step (`StepStatus::Failed`).
    Failed,
}

impl PlanStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            PlanStatus::Draft => "draft",
            PlanStatus::PendingApproval => "pending_approval",
            PlanStatus::Approved => "approved",
            PlanStatus::Executing => "executing",
            PlanStatus::Blocked => "blocked",
            PlanStatus::Completed => "completed",
            PlanStatus::Rejected => "rejected",
            PlanStatus::Superseded => "superseded",
            PlanStatus::Cancelled => "cancelled",
            PlanStatus::NeedsReapproval => "needs_reapproval",
            PlanStatus::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Result<PlanStatus> {
        [
            PlanStatus::Draft,
            PlanStatus::PendingApproval,
            PlanStatus::Approved,
            PlanStatus::Executing,
            PlanStatus::Blocked,
            PlanStatus::Completed,
            PlanStatus::Rejected,
            PlanStatus::Superseded,
            PlanStatus::Cancelled,
            PlanStatus::NeedsReapproval,
            PlanStatus::Failed,
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
                | PlanStatus::Failed
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

/// One milestone of a plan: a named marker on the way to the goal.
/// Stored as a JSON array in `plans.milestones_json` (no separate table),
/// read and written with the plan as a whole.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Milestone {
    pub title: String,
    #[serde(default)]
    pub done: bool,
}

impl Milestone {
    pub fn new(title: &str) -> Self {
        Milestone {
            title: title.into(),
            done: false,
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
    /// The durable goal this plan serves, as distinct from `task`
    /// (the prompt that produced the plan). May be empty.
    pub goal: String,
    pub status: PlanStatus,
    /// §10.2 revision (M1): a user change commits a new version; the
    /// task's `plan_version` mirrors it, and approvals bound to an old
    /// version no longer apply.
    pub version: u64,
    /// Model that produced the plan (`""` when hand-written / edited).
    pub model: String,
    pub steps: Vec<PlanStep>,
    /// Id of the step execution has reached (`None` = none yet / finished).
    /// Step ids (not indices): an index goes stale the moment a step is
    /// inserted or removed before it, while an id either still exists or
    /// is detectably dangling — the runner falls back to the first
    /// unfinished step when it does not.
    pub current_step: Option<String>,
    /// Milestones, in order. Read and written with the plan as a whole.
    pub milestones: Vec<Milestone>,
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

    /// Canonical JSON of the milestones (what the store writes).
    pub fn milestones_json(milestones: &[Milestone]) -> String {
        serde_json::to_string(milestones).unwrap_or_else(|_| "[]".into())
    }

    /// Parse stored milestones; corrupt JSON reads as no milestones
    /// (same tolerant rule as step arguments / files).
    pub fn parse_milestones_json(raw: &str) -> Vec<Milestone> {
        serde_json::from_str(raw).unwrap_or_default()
    }
}

/// Outcome of one executed step, for the run report.
#[derive(Clone, Debug)]
pub struct StepOutcome {
    pub step_id: String,
    pub status: StepStatus,
    pub output: String,
    pub approval_id: Option<String>,
    /// Receipt of the step's side effect, when the run verified one
    /// (M1: the action-ledger key of the executed approval, or the
    /// verifier's evidence id). `None` means no receipt was checked.
    pub receipt: Option<String>,
    /// The verifier could not tell whether the side effect happened
    /// (M1): the step failed, but its external state is unknown — the
    /// caller must show it, never silently retry it.
    pub ambiguous: bool,
}

impl StepOutcome {
    /// A step with no receipt check (the pre-M1 shape).
    pub fn plain(step_id: String, status: StepStatus, output: String, approval_id: Option<String>) -> Self {
        StepOutcome { step_id, status, output, approval_id, receipt: None, ambiguous: false }
    }
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
            PlanStatus::Blocked,
            PlanStatus::Failed,
            PlanStatus::NeedsReapproval,
            PlanStatus::Completed,
        ] {
            assert_eq!(PlanStatus::parse(s.as_str()).unwrap(), s);
        }
        assert!(PlanStatus::Completed.is_terminal());
        assert!(PlanStatus::Failed.is_terminal());
        assert!(!PlanStatus::Blocked.is_terminal());
        assert!(!PlanStatus::Approved.is_terminal());
        assert!(PlanStatus::parse("bogus").is_err());
    }

    #[test]
    fn milestone_json_roundtrip() {
        let ms = vec![
            Milestone::new("初稿"),
            Milestone {
                title: "终稿".into(),
                done: true,
            },
        ];
        let raw = Plan::milestones_json(&ms);
        assert_eq!(Plan::parse_milestones_json(&raw), ms);
        assert_eq!(Plan::milestones_json(&[]), "[]");
        assert!(Plan::parse_milestones_json("").is_empty());
        assert!(Plan::parse_milestones_json("not json").is_empty());
    }
}
