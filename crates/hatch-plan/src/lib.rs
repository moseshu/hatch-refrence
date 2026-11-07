//! `hatch-plan` — plan mode.
//!
//! The model first produces a *structured* plan (typed steps, not free
//! text). A plan is persisted in SQLite, approved by the user as a distinct,
//! auditable decision, and only then executed — one step at a time, through
//! the existing [`hatch_tools::dispatch_call`] gate path. Before each step
//! the runner verifies the step still matches the approved snapshot; any
//! deviation stops execution and sends the plan back for re-approval.
//!
//! The approval itself goes through the approval queue
//! ([`approval::submit_plan_approval`]): entering `pending_approval`
//! submits a `plan_approve` entry, and the plan moves to `approved` only
//! when that entry is decided `Approved`. There is no second path.
//!
//! * [`plan`]     — typed plan / step structs and the state machine types.
//! * [`validate`] — parse model JSON into steps, validate against the tool
//!   registry and the workspace guard, detect deviations from an approved
//!   snapshot.
//! * [`store`]    — [`PlanStore`], the SQLite persistence and state machine.
//! * [`planner`]  — ask the model for a plan for a task.
//! * [`execute`]  — run an approved plan step by step.
//! * [`approval`] — plan approvals via the approval queue.

pub mod approval;
pub mod execute;
pub mod plan;
pub mod planner;
pub mod store;
pub mod validate;

mod transitions;

pub use approval::{
    latest_plan_approval, pending_plan_approval, reject_pending_plan_approvals,
    submit_plan_approval, PlanApproveExecutor, PLAN_APPROVAL_KIND,
};
pub use execute::{run_plan, RunDeps, RunOutcome, RunReport};
pub use plan::{
    Deviation, Plan, PlanStatus, PlanStep, PlanSummary, RiskLevel, StepEdit, StepOutcome,
    StepStatus,
};
pub use planner::request_plan;
pub use store::PlanStore;
pub use validate::{check_step, detect_deviation, parse_plan_json, validate_step};
