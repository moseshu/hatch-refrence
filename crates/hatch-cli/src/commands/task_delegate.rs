//! Durable delegation, CLI + worker side (task 7).
//!
//! - `task delegate` spawns a child task for one parent step.
//! - `task consume` consumes every child that reported done.
//! - `run_join` is the `delegate-join` kind's worker path: consume,
//!   apply the partial-failure rules, block while children are still
//!   out, finish when everything is consumed.
//! - `report_child_if_delegated` is the hook a child worker calls
//!   when its task reaches a terminal state: the child's structured
//!   result goes into the delegation row and the parent is woken.
//!   Best-effort by design — a failure to report never fails the
//!   child's own run; the parent can still consume on its next run.

use super::task_worker::RunnerOutcome;
use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use hatch_store::{params, Db, DbResultExt};
use hatch_tasks::{
    has_pending_children, BlockedKind, ChildResult, ChildSpec, DelegationStore, EvidenceRef,
    ExecStatus, FileWrite, Lease, TaskContextStore, TaskStatus, TaskStore, UnitStore,
};
use serde_json::json;

/// The `delegate-join` kind's worker path. Consumes ready children
/// (with the parent lease, so the bound steps complete in the same
/// transaction), applies the partial-failure rules, and either blocks
/// while children are still out or reports done. Makes zero model
/// calls — the join is bookkeeping, not generation.
pub(crate) fn run_join(
    db: &Db,
    task_id: &str,
    lease: &Lease,
    now: &Timestamp,
) -> Result<RunnerOutcome> {
    let delegations = DelegationStore::new(db);
    let consumed = delegations.consume_ready(task_id, Some(lease), now)?;
    let _ = delegations.complete_consumed_steps(task_id, lease, now)?;
    let failures = delegations.handle_failed(task_id, Some(lease), now)?;
    if let Some(reason) = failures.blocked_reason {
        return Ok(RunnerOutcome {
            blocked: Some(reason),
            blocked_kind: Some(BlockedKind::Verification),
            ..Default::default()
        });
    }
    let remaining = delegations.list_for_parent(task_id)?;
    if has_pending_children(&remaining) {
        let names: Vec<String> = remaining
            .iter()
            .filter(|d| {
                matches!(
                    d.status,
                    hatch_tasks::DelegationStatus::Delegated
                        | hatch_tasks::DelegationStatus::Running
                )
            })
            .map(|d| format!("{}（{}）", d.step_id, d.child_task))
            .collect();
        return Ok(RunnerOutcome {
            blocked: Some(format!("等待子任务：{}", names.join("、"))),
            blocked_kind: Some(BlockedKind::Children),
            ..Default::default()
        });
    }
    if !consumed.conflicts.is_empty() {
        // Conflicts are recorded on the parent (open items + events)
        // by the consume itself; the join still finishes — the
        // conflict is a recorded gap, not a lost write.
        let _ = consumed;
    }
    Ok(RunnerOutcome::default())
}

/// A child worker calls this when its task reaches a terminal state.
/// If the task is a delegated child, its structured result is
/// reported (done or failed) and the parent is woken. A task that is
/// not a child, or not terminal yet, is a silent no-op.
pub(crate) fn report_child_if_delegated(db: &Db, task_id: &str, now: &Timestamp) -> Result<()> {
    let delegations = DelegationStore::new(db);
    let Some(d) = delegations.get_by_child(task_id)? else {
        return Ok(());
    };
    if d.status.is_terminal() {
        return Ok(());
    }
    let store = TaskStore::new(db);
    let Some(task) = store.get(task_id)? else {
        return Ok(());
    };
    if !task.status.is_terminal() {
        return Ok(());
    }
    let result = build_child_result(db, task_id)?;
    let failure_kind = if task.status == TaskStatus::Failed {
        failed_step_kind(db, task_id)?.unwrap_or_else(|| "permanent".to_string())
    } else {
        String::new()
    };
    // Best-effort: the child's own run already finished; a report
    // failure must not turn it into an error. The parent can still
    // consume on its next run (the delegation is done, the result is
    // in the child's own records).
    let _ = delegations.report_result(task_id, &result, &failure_kind, now);
    Ok(())
}

/// The failure kind of a child's failed step, if one was recorded
/// (task 6 mirrors the plan's kind onto the step).
fn failed_step_kind(db: &Db, task_id: &str) -> Result<Option<String>> {
    let steps = UnitStore::new(db).list_steps(task_id)?;
    Ok(steps
        .iter()
        .find(|s| s.status == ExecStatus::Failed && !s.failure_kind.is_empty())
        .map(|s| s.failure_kind.clone()))
}

/// Build a child's structured result from its own durable records:
/// its saved context (evidence, facts, open items) and its done
/// steps' outputs. Mail ids cited in the brief become evidence
/// references; an artifact write becomes a versioned file write
/// (base "" = a new file, the receipt as the new version).
pub(crate) fn build_child_result(db: &Db, task_id: &str) -> Result<ChildResult> {
    let mut result = ChildResult::default();
    if let Some(ctx) = TaskContextStore::new(db).load(task_id)? {
        result.evidence = ctx.evidence;
        result.conclusions = ctx.facts;
        result.open_items = ctx.open_items;
    }
    let steps = UnitStore::new(db).list_steps(task_id)?;
    for s in steps.iter().filter(|s| s.status == ExecStatus::Done) {
        if !s.output.is_empty() {
            result
                .conclusions
                .push(format!("{}：{}", s.title, truncate(&s.output, 500)));
            for id in extract_mail_ids(&s.output) {
                let evidence = EvidenceRef::new("mail", &id, "简报引用");
                if !result
                    .evidence
                    .iter()
                    .any(|e| e.kind == "mail" && e.id == id)
                {
                    result.evidence.push(evidence);
                }
            }
        }
        if s.tool == "artifact.write" {
            if let Some(path) = artifact_path(s) {
                result.writes.push(FileWrite {
                    path,
                    base_version: String::new(),
                    new_version: if s.receipt_id.is_empty() {
                        format!("step:{}", s.step_id)
                    } else {
                        s.receipt_id.clone()
                    },
                    hash: s.receipt_id.clone(),
                });
            }
        }
    }
    if result.summary.is_empty() {
        result.summary = steps
            .iter()
            .rev()
            .find(|s| s.status == ExecStatus::Done)
            .map(|s| truncate(&s.output, 200))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "子任务完成".to_string());
    }
    Ok(result)
}

/// The file an artifact step wrote: the output says `简报已写入
/// <path>`; fall back to the step arguments' `path`.
fn artifact_path(s: &hatch_tasks::TaskStep) -> Option<String> {
    if let Some(p) = s.output.strip_prefix("简报已写入 ") {
        let p = p.trim();
        if !p.is_empty() {
            return Some(p.to_string());
        }
    }
    s.arguments
        .get("path")
        .and_then(|v| v.as_str())
        .map(|p| p.to_string())
}

/// Mail ids cited in a text: `mail_` followed by lowercase letters
/// and digits (the brief's source citations). Byte-safe: matches are
/// found with `match_indices` (ASCII, so always on a boundary) and
/// the id run stops at the first non-`[a-z0-9]` byte, which is itself
/// a char boundary.
fn extract_mail_ids(text: &str) -> Vec<String> {
    let mut ids = Vec::new();
    for (i, _) in text.match_indices("mail_") {
        let mut j = i + 5;
        while j < text.len()
            && (text.as_bytes()[j].is_ascii_lowercase() || text.as_bytes()[j].is_ascii_digit())
        {
            j += 1;
        }
        if j > i + 5 {
            let id = text[i..j].to_string();
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    ids
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}

/// Spawn a child task for one parent step (CLI `task delegate`).
/// The parent gets a step row for the step (control-plane write, no
/// lease — like `task create`), so the plan card shows the delegated
/// step and the consume can complete it.
pub fn delegate_task(
    db: &Db,
    parent_id: &str,
    step_id: &str,
    kind: &str,
    title: &str,
    budget: u64,
    now: &Timestamp,
) -> Result<String> {
    let store = TaskStore::new(db);
    let parent = store
        .get(parent_id)?
        .ok_or_else(|| Error::NotFound(format!("task {parent_id} 不存在")))?;
    if parent.status.is_terminal() {
        return Err(Error::Invalid(format!(
            "task {parent_id} 已{}，不能再派生子任务",
            parent.status.as_str()
        )));
    }
    ensure_delegation_step(db, parent_id, step_id, title, now)?;
    let d = DelegationStore::new(db).spawn(
        parent_id,
        step_id,
        ChildSpec::new(kind, title, "cli", budget),
        now,
    )?;
    Ok(format!(
        "子任务 {} 已派生（步骤 {step_id}，kind {kind}，预算 {} 次模型调用）；用 `hatch task run --task {}` 执行它",
        d.child_task,
        if budget == 0 { "不限".to_string() } else { budget.to_string() },
        d.child_task
    ))
}

/// Consume every child that reported done (CLI `task consume`).
/// No lease: the merge and the marker commit, the bound steps stay
/// pending until the next worker run completes them.
pub fn consume_task(db: &Db, parent_id: &str, now: &Timestamp) -> Result<String> {
    let store = TaskStore::new(db);
    store
        .get(parent_id)?
        .ok_or_else(|| Error::NotFound(format!("task {parent_id} 不存在")))?;
    let outcome = DelegationStore::new(db).consume_ready(parent_id, None, now)?;
    let mut msg = format!(
        "已消费 {} 个子任务，已消费过 {} 个",
        outcome.consumed.len(),
        outcome.already.len()
    );
    if !outcome.conflicts.is_empty() {
        msg.push_str(&format!(
            "，{} 处写冲突已记录（未覆盖）",
            outcome.conflicts.len()
        ));
        for c in &outcome.conflicts {
            msg.push_str(&format!(
                "\n  冲突 {}：父任务已有版本 {}（{}），{} 试图写入版本 {}",
                c.path,
                c.current_version,
                c.current_writer,
                c.attempted_writer,
                c.attempted_version
            ));
        }
    }
    if !outcome.steps_pending.is_empty() {
        msg.push_str(&format!(
            "；{} 个步骤待下次运行完成（无租约）",
            outcome.steps_pending.len()
        ));
    }
    Ok(msg)
}

/// One step row for a delegated step, created idempotently
/// (control-plane write, no lease — the worker's `ensure_steps`
/// needs a lease and cannot run here).
fn ensure_delegation_step(
    db: &Db,
    parent_id: &str,
    step_id: &str,
    title: &str,
    now: &Timestamp,
) -> Result<()> {
    let ts = to_rfc3339(now);
    let args = json!({});
    db.transaction(|c| {
        let idx: i64 = c
            .query_row(
                "SELECT COALESCE(MAX(idx), -1) + 1 FROM task_steps WHERE task_id = ?1",
                params![parent_id],
                |r| r.get(0),
            )
            .db()?;
        c.execute(
            "INSERT OR IGNORE INTO task_steps(task_id, idx, step_id, title, tool, arguments, expected, status, attempts, input_refs, approval_id, receipt_id, output, failure_kind, created_at, updated_at) VALUES(?1,?2,?3,?4,'delegate.child',?5,'子任务完成并已合并','pending',0,'[]','','','',?6,?6)",
            params![parent_id, idx, step_id, title, serde_json::to_string(&args).unwrap_or_else(|_| "{}".into()), ts],
        )
        .db()?;
        Ok(())
    })?;
    Ok(())
}
