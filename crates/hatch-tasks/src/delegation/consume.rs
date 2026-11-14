//! Durable delegation, part 3: consuming results exactly once.

use super::types::{ConsumeOutcome, DelegationStatus, MergeConflict};
use super::DelegationStore;
use crate::events::insert_event;
use crate::task::Lease;
use crate::TaskContext;
use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use hatch_store::{params, DbResultExt, OptionalExtension};

impl<'a> DelegationStore<'a> {
    pub fn consume_ready(
        &self,
        parent_task: &str,
        lease: Option<&Lease>,
        now: &Timestamp,
    ) -> Result<ConsumeOutcome> {
        let mut outcome = ConsumeOutcome::default();
        for d in self.list_for_parent(parent_task)? {
            if d.status != DelegationStatus::Done && d.status != DelegationStatus::Consumed {
                continue;
            }
            let one = self.consume_one(parent_task, &d.child_task, lease, now)?;
            outcome.consumed.extend(one.consumed);
            outcome.already.extend(one.already);
            outcome.conflicts.extend(one.conflicts);
            outcome.steps_pending.extend(one.steps_pending);
        }
        Ok(outcome)
    }

    /// Consume one child, exactly once. The merge (evidence into the
    /// parent context, writes against the resource versions), the
    /// consumption marker, the parent event, the delegation move to
    /// `consumed`, and — when a lease is given and matches — the
    /// parent step's completion commit in one transaction. A replay
    /// finds the marker and returns `already` without touching
    /// anything.
    pub fn consume_one(
        &self,
        parent_task: &str,
        child_task: &str,
        lease: Option<&Lease>,
        now: &Timestamp,
    ) -> Result<ConsumeOutcome> {
        let mut outcome = ConsumeOutcome::default();
        let delegation = self
            .get_by_child(child_task)?
            .ok_or_else(|| Error::NotFound(format!("no delegation for child {child_task}")))?;
        if delegation.parent_task != parent_task {
            return Err(Error::Invalid(format!(
                "child {child_task} does not belong to {parent_task}"
            )));
        }
        if delegation.status == DelegationStatus::Consumed {
            outcome.already.push(child_task.to_string());
            return Ok(outcome);
        }
        if delegation.status != DelegationStatus::Done {
            return Err(Error::Invalid(format!(
                "child {child_task} is {}, not done",
                delegation.status.as_str()
            )));
        }
        let result = delegation.result.clone().unwrap_or_default();
        let ts = to_rfc3339(now);
        let parent = parent_task.to_string();
        let child = child_task.to_string();
        let step = delegation.step_id.clone();
        let summary = result.summary.clone();
        let lease = lease.cloned();
        // (consumed by this call?, conflicts). The transaction is
        // BEGIN IMMEDIATE and holds the lock, so a concurrent consume
        // of the same child waits, then finds the marker.
        let (consumed_now, conflicts) = self.db.transaction(|c| {
            // The marker first: any replay after this commit sees it.
            let marked: i64 = c
                .query_row(
                    "SELECT COUNT(*) FROM task_consumptions WHERE parent_task = ?1 AND child_task = ?2",
                    params![parent, child],
                    |r| r.get(0),
                )
                .db()?;
            if marked > 0 {
                return Ok((false, Vec::new()));
            }
            // Merge the evidence into the parent context (dedup by
            // kind+id; conclusions and open items dedup by text).
            let (mut ctx, has_ctx) = load_context(c, &parent)?;
            for e in &result.evidence {
                if !ctx.evidence.iter().any(|x| x.kind == e.kind && x.id == e.id) {
                    ctx.evidence.push(e.clone());
                }
            }
            for f in &result.conclusions {
                if !ctx.facts.contains(f) {
                    ctx.facts.push(f.clone());
                }
            }
            for o in &result.open_items {
                if !ctx.open_items.contains(o) {
                    ctx.open_items.push(o.clone());
                }
            }
            // Merge the writes: a write applies only when the version
            // the child read is still the version the parent knows.
            let mut conflicts = Vec::new();
            for w in &result.writes {
                let current: Option<(String, String)> = c
                    .query_row(
                        "SELECT version, updated_by FROM task_resource_versions WHERE task_id = ?1 AND resource = ?2",
                        params![parent, w.path],
                        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                    )
                    .optional()
                    .db()?;
                let applies = match &current {
                    None => w.base_version.is_empty(),
                    Some((v, _)) => *v == w.base_version,
                };
                if applies {
                    c.execute(
                        "INSERT INTO task_resource_versions(task_id, resource, version, hash, updated_by, updated_at) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(task_id, resource) DO UPDATE SET version = excluded.version, hash = excluded.hash, updated_by = excluded.updated_by, updated_at = excluded.updated_at",
                        params![parent, w.path, w.new_version, w.hash, child, ts],
                    )
                    .db()?;
                } else {
                    let (cv, cw) = current.unwrap_or_default();
                    conflicts.push(MergeConflict {
                        path: w.path.clone(),
                        current_version: cv,
                        current_writer: cw,
                        attempted_version: w.new_version.clone(),
                        attempted_writer: child.clone(),
                    });
                }
            }
            // Conflicts are recorded, never silently resolved: the
            // parent keeps the conflict as an open item, so the human
            // (or the next plan) can decide which version wins.
            for conflict in &conflicts {
                let note = format!(
                    "写冲突：{} 已有版本 {}（{}），{} 试图写入版本 {}（未覆盖）",
                    conflict.path,
                    conflict.current_version,
                    conflict.current_writer,
                    conflict.attempted_writer,
                    conflict.attempted_version
                );
                if !ctx.open_items.contains(&note) {
                    ctx.open_items.push(note);
                }
            }
            save_context(c, &ctx, has_ctx, &ts)?;
            // The marker, the event, and the delegation move.
            let event = insert_event(
                c,
                &parent,
                "child_consumed",
                &step,
                &format!("已消费子任务 {child}：{summary}"),
                &ts,
            )?;
            c.execute(
                "INSERT INTO task_consumptions(parent_task, child_task, event_id, consumed_at) VALUES(?1,?2,?3,?4)",
                params![parent, child, event.seq, ts],
            )
            .db()?;
            c.execute(
                "UPDATE task_delegations SET status = 'consumed', consumed_at = ?2, updated_at = ?2 WHERE child_task = ?1",
                params![child, ts],
            )
            .db()?;
            // Complete the parent step bound to this delegation, when
            // the caller holds the parent's lease. Without a lease
            // the step stays pending — the next worker run completes
            // it from the stored result (see `complete_consumed_steps`).
            if let Some(l) = &lease {
                let (owner, generation): (String, i64) = c
                    .query_row(
                        "SELECT lease_owner, lease_generation FROM tasks WHERE id = ?1",
                        params![parent],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .db()?;
                if owner == l.owner && generation == l.generation as i64 {
                    let changed = c.execute(
                        "UPDATE task_steps SET status = 'done', output = ?3, updated_at = ?4 WHERE task_id = ?1 AND step_id = ?2 AND status != 'done'",
                        params![parent, step, summary, ts],
                    )
                    .db()?;
                    if changed > 0 {
                        insert_event(c, &parent, "step_done", &step, &summary, &ts)?;
                    }
                }
            }
            Ok((true, conflicts))
        })?;
        if consumed_now {
            outcome.consumed.push(child_task.to_string());
        } else {
            outcome.already.push(child_task.to_string());
        }
        outcome.conflicts = conflicts;
        if lease.is_none() && !step.is_empty() {
            outcome.steps_pending.push(step);
        }
        Ok(outcome)
    }

    /// Complete the steps of delegations already consumed (the
    /// no-lease consume left them pending). Called by the worker,
    /// which holds the parent lease.
    pub fn complete_consumed_steps(
        &self,
        parent_task: &str,
        lease: &Lease,
        now: &Timestamp,
    ) -> Result<Vec<String>> {
        let mut done = Vec::new();
        for d in self.list_for_parent(parent_task)? {
            if d.status != DelegationStatus::Consumed || d.step_id.is_empty() {
                continue;
            }
            let summary = d
                .result
                .as_ref()
                .map(|r| r.summary.clone())
                .unwrap_or_default();
            let ts = to_rfc3339(now);
            let parent = parent_task.to_string();
            let step = d.step_id.clone();
            let lease = lease.clone();
            let mut changed_step = false;
            self.db.transaction(|c| {
                let (owner, generation): (String, i64) = c
                    .query_row(
                        "SELECT lease_owner, lease_generation FROM tasks WHERE id = ?1",
                        params![parent],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .db()?;
                if owner != lease.owner || generation != lease.generation as i64 {
                    return Ok(());
                }
                let changed = c
                    .execute(
                        "UPDATE task_steps SET status = 'done', output = ?3, updated_at = ?4 WHERE task_id = ?1 AND step_id = ?2 AND status != 'done'",
                        params![parent, step, summary, ts],
                    )
                    .db()?;
                if changed > 0 {
                    insert_event(c, &parent, "step_done", &step, &summary, &ts)?;
                    changed_step = true;
                }
                Ok(())
            })?;
            if changed_step {
                done.push(step);
            }
        }
        Ok(done)
    }
}

/// Load the parent context inside a transaction (raw SQL: the store
/// helper takes its own connection, which a transaction cannot use).
/// A missing row loads as an empty context.
pub(crate) fn load_context(
    c: &hatch_store::Connection,
    task_id: &str,
) -> Result<(TaskContext, bool)> {
    let row: Option<(String, String, String, String, String)> = c
        .query_row(
            "SELECT goal, facts_json, constraints_json, evidence_json, open_items_json FROM task_context WHERE task_id = ?1",
            params![task_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                ))
            },
        )
        .optional()
        .db()?;
    match row {
        None => Ok((
            TaskContext {
                task_id: task_id.to_string(),
                ..Default::default()
            },
            false,
        )),
        Some((goal, facts, constraints, evidence, open_items)) => Ok((
            TaskContext {
                task_id: task_id.to_string(),
                goal,
                facts: serde_json::from_str(&facts).unwrap_or_default(),
                constraints: serde_json::from_str(&constraints).unwrap_or_default(),
                evidence: serde_json::from_str(&evidence).unwrap_or_default(),
                open_items: serde_json::from_str(&open_items).unwrap_or_default(),
            },
            true,
        )),
    }
}

pub(crate) fn save_context(
    c: &hatch_store::Connection,
    ctx: &TaskContext,
    exists: bool,
    ts: &str,
) -> Result<()> {
    let facts = serde_json::to_string(&ctx.facts).unwrap_or_else(|_| "[]".into());
    let constraints = serde_json::to_string(&ctx.constraints).unwrap_or_else(|_| "[]".into());
    let evidence = serde_json::to_string(&ctx.evidence).unwrap_or_else(|_| "[]".into());
    let open_items = serde_json::to_string(&ctx.open_items).unwrap_or_else(|_| "[]".into());
    if exists {
        c.execute(
            "UPDATE task_context SET goal = ?2, facts_json = ?3, constraints_json = ?4, evidence_json = ?5, open_items_json = ?6, updated_at = ?7 WHERE task_id = ?1",
            params![ctx.task_id, ctx.goal, facts, constraints, evidence, open_items, ts],
        )
        .db()?;
    } else {
        c.execute(
            "INSERT INTO task_context(task_id, goal, facts_json, constraints_json, evidence_json, open_items_json, updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![ctx.task_id, ctx.goal, facts, constraints, evidence, open_items, ts],
        )
        .db()?;
    }
    Ok(())
}
