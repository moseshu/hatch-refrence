//! One turn's plan bookkeeping: the bound plan (snapshot at turn start),
//! the calls made so far, and the stop-on-drift enforcement.
//!
//! Kept apart from [`crate::reanchor`] so that file stays the pure
//! coverage / rendering logic. The agent loop owns one `TurnPlan` per
//! turn (or none, when the turn is unbound).

use crate::reanchor::{
    check_coverage, check_drift, is_anchor_step, render_anchor, CallRef, Drift, PlanTurnReport,
};
use crate::report::StopReason;
use crate::session::SessionStore;
use crate::unanswered;
use hatch_core::time::Timestamp;
use hatch_core::{Result, Trust};
use hatch_llm::ChatMessage;
use hatch_plan::{Plan, PlanStatus, PlanStore};
use hatch_store::Db;

/// A turn bound to a plan. The plan is a snapshot from turn start; a
/// plan that changes mid-turn is re-read next turn.
pub struct TurnPlan {
    plan: Plan,
    every: usize,
    calls: Vec<CallRef>,
}

impl TurnPlan {
    /// Bind a turn to a plan. Only `approved` / `executing` plans anchor
    /// a turn — anything else (draft, blocked, done, failed, …) means the
    /// turn runs unbound, exactly as before. A missing plan or a DB error
    /// also means unbound: plan bookkeeping must never fail a turn.
    pub fn bind(db: &Db, plan_id: &str, every: usize) -> Option<TurnPlan> {
        let plan = PlanStore::new(db).get(plan_id).ok().flatten()?;
        if !matches!(plan.status, PlanStatus::Approved | PlanStatus::Executing) {
            return None;
        }
        Some(TurnPlan {
            plan,
            every: every.max(1),
            calls: Vec::new(),
        })
    }

    pub fn plan_id(&self) -> &str {
        &self.plan.id
    }

    /// The anchor text for this step, or `None` on non-anchor steps.
    pub fn anchor_for_step(&self, step: usize) -> Option<String> {
        if !is_anchor_step(step, self.every) {
            return None;
        }
        let cov = check_coverage(&self.plan.steps, &self.calls);
        Some(render_anchor(&self.plan, &cov.covered))
    }

    /// Record one executed call (blocked-by-skill calls never ran, so the
    /// caller does not record them).
    pub fn note_call(&mut self, call: CallRef) {
        self.calls.push(call);
    }

    /// Drift check after a step. `None` means on track.
    pub fn after_step(&self) -> Option<Drift> {
        check_drift(&self.plan.steps, &self.calls, self.every)
    }

    /// Coverage summary for the report / receipt.
    pub fn report(&self, drift: Option<&str>) -> PlanTurnReport {
        let cov = check_coverage(&self.plan.steps, &self.calls);
        PlanTurnReport {
            plan_id: self.plan.id.clone(),
            covered: cov.covered,
            uncovered: cov.uncovered,
            off_plan: cov.off_plan,
            drift: drift.map(|d| d.to_string()),
        }
    }
}

/// Stop a turn on drift: flag the plan `needs_reapproval` through the
/// existing path (a fresh human decision is submitted — nothing here
/// approves anything), tell the user in the transcript, annotate any
/// still-pending approvals, and return the stop reason for the caller
/// to record. The turn stops even if flagging fails — the stop itself
/// is the enforcement; the flag is for the next turn.
pub fn stop_on_drift(
    db: &Db,
    sessions: &SessionStore,
    session: &str,
    approvals: &[String],
    plan_id: &str,
    drift: &Drift,
    now: &Timestamp,
) -> Result<StopReason> {
    let flagged = PlanStore::new(db).flag_deviation(plan_id, &drift.reason, now);
    let note = match &flagged {
        Ok(()) => format!(
            "（已偏离计划，停下等你决定：{}。计划已标记为需重新审批。）",
            drift.reason
        ),
        Err(e) => format!(
            "（已偏离计划，停下等你决定：{}。但标记需重新审批失败（{e}），请检查计划状态。）",
            drift.reason
        ),
    };
    sessions.append(session, &ChatMessage::assistant(&note), Trust::System, now)?;
    unanswered::annotate(db, sessions, session, approvals, now);
    Ok(StopReason::PlanDeviation(drift.reason.clone()))
}
