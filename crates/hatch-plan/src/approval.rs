//! Plan approvals go through the approval queue.
//!
//! A plan is never approved by flipping a row in the `plans` table alone:
//! entering `pending_approval` (or `needs_reapproval`) submits a
//! `plan_approve` entry to the [`ApprovalQueue`], and the plan moves to
//! `approved` only when that entry is decided `Approved` — through the same
//! `decide` + `execute_approved` path as every other approval
//! (`hatch plan approve`, `hatch approvals approve`, the product API).
//! [`PlanStore::approve`] refuses to apply a decision unless the newest
//! entry for this plan version exists and is `Approved`, so there is no
//! second path to approval.
//!
//! Linkage: the entry carries `kind = plan_approve`,
//! `origin = plan:<plan id>`, and a payload `{plan_id, steps_hash}`. The
//! idempotency key is a fresh random id per submission — deliberately NOT
//! derived from the plan version: timestamps are second-precision, so two
//! approval rounds for identical steps within one second would share a
//! derived key and the second round would silently reuse the first
//! round's already-decided entry, leaving the plan with no pending entry
//! to decide. The invariant that matters instead is "at most one pending
//! entry per plan": submitting rejects any still-pending entry for the
//! same plan first, and [`PlanStore::approve`] matches the newest entry
//! for the current steps hash ([`latest_plan_approval`]).
//!
//! The entry pins exactly what the user is asked to approve: the payload
//! carries the plan id and the hash of the current steps, and both
//! [`PlanApproveExecutor`] and [`PlanStore::approve`] refuse a decision
//! whose steps no longer match. Approving a plan approves the PLAN — it
//! does not pre-approve the steps' future side effects: every step still
//! goes through the dispatch gate (and its own approval queue) at
//! execution time.
//!
//! Class: [`ActionClass::DraftMessage`] — approving a plan is a local-only
//! decision; nothing leaves the machine when the plan is approved. No
//! grant can auto-decide a queued entry (grants only affect whether the
//! gate queues an action in the first place, and the sub-agent replay path
//! only replays approvals the child itself created), so a plan approval
//! can never be executed by a grant meant for something else.

use crate::plan::{Plan, PlanStatus};
use crate::store::PlanStore;
use hatch_core::time::Timestamp;
use hatch_core::{ids, Error, Result};
use hatch_policy::{ActionClass, Approval, ApprovalQueue, ApprovalStatus, NewApproval};
use hatch_store::Db;
use hatch_tools::{ActionExecutor, ExecContext};
use serde::Deserialize;
use serde_json::{json, Value};

/// Kind of the approval-queue entry that approves a plan.
pub const PLAN_APPROVAL_KIND: &str = "plan_approve";

/// Origin of a plan approval entry: `plan:<plan id>`.
pub fn plan_approval_origin(plan_id: &str) -> String {
    format!("plan:{plan_id}")
}

/// The newest `plan_approve` entry for the plan's CURRENT steps
/// (payload `steps_hash` matches), or `None`. `list` returns newest
/// first, so the first match is the latest decision for this version —
/// an older decision for identical steps never authorises a newer round
/// while a newer entry for the same steps exists.
pub fn latest_plan_approval(db: &Db, plan: &Plan) -> Result<Option<Approval>> {
    let origin = plan_approval_origin(&plan.id);
    let hash = Plan::steps_hash(&plan.steps);
    // The current round's entry is the one still awaiting a decision:
    // a reset-only replan keeps the steps hash, so an old (already
    // executed) entry for the same hash must not shadow the new one.
    // When no round is open, fall back to the newest matching entry
    // (the `approve` check reads its status to refuse a stale call).
    let mut fallback: Option<Approval> = None;
    for a in ApprovalQueue::new(db).list(None)? {
        if a.kind == PLAN_APPROVAL_KIND
            && a.origin == origin
            && a.payload.get("steps_hash").and_then(|v| v.as_str()) == Some(hash.as_str())
        {
            if a.status == ApprovalStatus::Pending {
                return Ok(Some(a));
            }
            if fallback.is_none() {
                fallback = Some(a);
            }
        }
    }
    Ok(fallback)
}

/// Reject every still-pending `plan_approve` entry for a plan.
/// Best-effort: a failure only means a stale entry lingers in the queue
/// (its executor fails closed on the hash check), never a crash.
pub fn reject_pending_plan_approvals(db: &Db, plan_id: &str, now: &Timestamp) {
    let Ok(list) = ApprovalQueue::new(db).list(Some(ApprovalStatus::Pending)) else {
        return;
    };
    let origin = plan_approval_origin(plan_id);
    let q = ApprovalQueue::new(db);
    for a in list {
        if a.kind == PLAN_APPROVAL_KIND
            && a.origin == origin
            && q.decide(&a.id, false, "计划已更新，旧审批作废", now)
                .is_err()
        {
            let _ = db.audit(
                &origin,
                "plan_approval_reject_failed",
                &format!("旧审批 {} 作废失败", a.id),
            );
        }
    }
}

/// The still-pending `plan_approve` entry for a plan, if any.
pub fn pending_plan_approval(db: &Db, plan_id: &str) -> Result<Option<Approval>> {
    let origin = plan_approval_origin(plan_id);
    for a in ApprovalQueue::new(db).list(Some(ApprovalStatus::Pending))? {
        if a.kind == PLAN_APPROVAL_KIND && a.origin == origin {
            return Ok(Some(a));
        }
    }
    Ok(None)
}

/// Submit the approval-queue entry for a plan that just entered
/// `pending_approval` / `needs_reapproval`. Rejects any still-pending
/// entries for the same plan first, so the queue never shows two live
/// approvals for one plan. Returns the entry id. The idempotency key is
/// fresh per submission (see the module docs): every round gets its own
/// entry, so a plan in `pending_approval` / `needs_reapproval` always
/// has exactly one pending entry to decide.
/// The preview lists step titles and tools, never arguments — arguments
/// can hold data the user should not have to re-read in an approval, and
/// the payload pins the steps by hash instead.
pub fn submit_plan_approval(db: &Db, plan: &Plan, now: &Timestamp) -> Result<String> {
    reject_pending_plan_approvals(db, &plan.id, now);
    let steps_hash = Plan::steps_hash(&plan.steps);
    let key = ids::random_id("plan-approve");
    let mut preview = format!("批准计划：{}\n", plan.task);
    for (i, s) in plan.steps.iter().enumerate() {
        preview.push_str(&format!(
            "{}. [{}] {} — {}（风险 {}）\n",
            i + 1,
            s.id,
            s.title,
            s.tool,
            s.risk.as_str()
        ));
    }
    preview.push_str("批准后方可执行；各步骤执行时仍需逐项过审批。");
    let na = NewApproval {
        idempotency_key: key,
        kind: PLAN_APPROVAL_KIND.into(),
        class: ActionClass::DraftMessage,
        title: format!("批准计划：{}", plan.task),
        preview,
        payload: json!({"plan_id": plan.id, "steps_hash": steps_hash}),
        reasons: vec!["计划批准后方可执行；各步骤执行时仍需逐项过审批".into()],
        origin: plan_approval_origin(&plan.id),
        ttl: chrono::Duration::hours(72),
    };
    Ok(ApprovalQueue::new(db).submit(&na, now)?.id().to_string())
}

/// Payload of a `plan_approve` approval entry.
#[derive(Deserialize)]
struct PlanApprovalPayload {
    plan_id: String,
    steps_hash: String,
}

/// Executor for `plan_approve` entries: applying an approved decision
/// flips the plan to `approved` (snapshotting the exact steps). It never
/// executes a step — steps run later, one by one, through the dispatch
/// gate. Created with the name recorded as the decider (the configured
/// user name, or `cli` when unset).
pub struct PlanApproveExecutor {
    decided_by: String,
}

impl PlanApproveExecutor {
    pub fn new(decided_by: impl Into<String>) -> Self {
        PlanApproveExecutor {
            decided_by: decided_by.into(),
        }
    }
}

impl ActionExecutor for PlanApproveExecutor {
    fn kind(&self) -> &'static str {
        PLAN_APPROVAL_KIND
    }

    fn execute(&self, payload: &Value, ctx: &ExecContext) -> Result<String> {
        let p: PlanApprovalPayload = serde_json::from_value(payload.clone())
            .map_err(|e| Error::Invalid(format!("plan_approve payload 无效：{e}")))?;
        let store = PlanStore::new(ctx.db);
        let plan = store
            .get(&p.plan_id)?
            .ok_or_else(|| Error::NotFound(format!("plan {} 不存在", p.plan_id)))?;
        if !matches!(
            plan.status,
            PlanStatus::PendingApproval | PlanStatus::NeedsReapproval
        ) {
            return Err(Error::Invalid(format!(
                "plan {} 处于 {}，不能批准",
                p.plan_id,
                plan.status.as_str()
            )));
        }
        // The decision covers exactly these steps: refuse anything else.
        if Plan::steps_hash(&plan.steps) != p.steps_hash {
            return Err(Error::Invalid(format!(
                "plan {} 的步骤在提交审批后变了，批准失效；请重新提交审批",
                p.plan_id
            )));
        }
        let approval = ApprovalQueue::new(ctx.db).get(ctx.approval_id)?;
        store.approve(&p.plan_id, &self.decided_by, &approval.note, &ctx.now)?;
        Ok(format!("计划 {} 已批准", p.plan_id))
    }
}
