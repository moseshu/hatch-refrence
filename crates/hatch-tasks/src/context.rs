//! Long-task context (task 6): the loadable state a long task carries
//! across turns, compressions and restarts.
//!
//! One `task_context` row per task: the delivery goal, the confirmed
//! facts, the hard constraints (including permission constraints —
//! what the task must never do), the evidence references, and the
//! unfinished items. The §10.3 invariant holds because this table is
//! read from the ledger, never from a summary: a corrupted summary
//! cannot change it, and a restart reloads exactly what was saved.
//!
//! The remaining budget is deliberately NOT stored here. It is derived
//! from the ledger (`tasks.max_model_calls` minus the units'
//! `model_calls` sum), so a replan can never reset the budget by
//! rewriting this row — the replan inherits what is left.

use crate::unit::UnitStore;
use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use hatch_store::{params, Db, DbResultExt, OptionalExtension};
use serde::{Deserialize, Serialize};

/// One evidence reference: what kind of thing it is (`mail`,
/// `calendar`, `file`, `approval`, `receipt`, …), its stable id, and
/// a short human-readable detail. The id is what a later step
/// re-reads; the detail is only for display.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvidenceRef {
    pub kind: String,
    pub id: String,
    pub detail: String,
}

impl EvidenceRef {
    pub fn new(kind: &str, id: &str, detail: &str) -> Self {
        EvidenceRef {
            kind: kind.to_string(),
            id: id.to_string(),
            detail: detail.to_string(),
        }
    }
}

/// The long-task context of one task. JSON lists, so an old row
/// missing a field still loads (tolerant reads below).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskContext {
    pub task_id: String,
    pub goal: String,
    pub facts: Vec<String>,
    pub constraints: Vec<String>,
    pub evidence: Vec<EvidenceRef>,
    pub open_items: Vec<String>,
}

/// The context plus its budget position, loaded in one call: what the
/// task is, what it must not do, what it has proven, what is left,
/// and how many model calls it may still spend (`None` = unlimited).
#[derive(Clone, Debug, PartialEq)]
pub struct TaskContextView {
    pub context: TaskContext,
    pub model_calls_used: u64,
    pub remaining_budget: Option<u64>,
}

pub struct TaskContextStore<'a> {
    db: &'a Db,
}

impl<'a> TaskContextStore<'a> {
    pub fn new(db: &'a Db) -> Self {
        TaskContextStore { db }
    }

    /// Save the whole context (upsert). The caller owns the merge:
    /// adds below are the idempotent way to grow one field at a time.
    /// The task must exist — a context row for an unknown task id is
    /// an orphan, refused here instead of saved silently.
    pub fn save(&self, ctx: &TaskContext, now: &Timestamp) -> Result<()> {
        check_task_exists(self.db, &ctx.task_id)?;
        let c = self.db.conn();
        c.execute(
            "INSERT INTO task_context(task_id, goal, facts_json, constraints_json, evidence_json, open_items_json, updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(task_id) DO UPDATE SET goal = excluded.goal, facts_json = excluded.facts_json, constraints_json = excluded.constraints_json, evidence_json = excluded.evidence_json, open_items_json = excluded.open_items_json, updated_at = excluded.updated_at",
            params![
                ctx.task_id,
                ctx.goal,
                serde_json::to_string(&ctx.facts).unwrap_or_else(|_| "[]".into()),
                serde_json::to_string(&ctx.constraints).unwrap_or_else(|_| "[]".into()),
                serde_json::to_string(&ctx.evidence).unwrap_or_else(|_| "[]".into()),
                serde_json::to_string(&ctx.open_items).unwrap_or_else(|_| "[]".into()),
                to_rfc3339(now),
            ],
        )
        .db()?;
        Ok(())
    }

    /// Load the context, if the task has one. Corrupt JSON in any one
    /// list reads as an empty list for that field (the other fields
    /// still load) — a damaged row degrades, it never fails the load.
    pub fn load(&self, task_id: &str) -> Result<Option<TaskContext>> {
        let c = self.db.conn();
        let row = c
            .query_row(
                "SELECT task_id, goal, facts_json, constraints_json, evidence_json, open_items_json FROM task_context WHERE task_id = ?1",
                params![task_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()
            .db()?;
        Ok(row.map(
            |(task_id, goal, facts, constraints, evidence, open_items)| TaskContext {
                task_id,
                goal,
                facts: serde_json::from_str(&facts).unwrap_or_default(),
                constraints: serde_json::from_str(&constraints).unwrap_or_default(),
                evidence: serde_json::from_str(&evidence).unwrap_or_default(),
                open_items: serde_json::from_str(&open_items).unwrap_or_default(),
            },
        ))
    }

    /// Load, or an empty context for the task (goal carried over when
    /// the caller passes one). Creating the row is `save`'s job.
    pub fn load_or_default(&self, task_id: &str, goal: &str) -> Result<TaskContext> {
        Ok(self.load(task_id)?.unwrap_or(TaskContext {
            task_id: task_id.to_string(),
            goal: goal.to_string(),
            ..Default::default()
        }))
    }

    /// The context plus its budget position. `max_model_calls` comes
    /// from the task row (0 = unlimited); the used count comes from
    /// the units ledger, so a replan inherits the remainder — it is
    /// never recomputed from a stored snapshot.
    pub fn view(&self, task_id: &str, max_model_calls: u64) -> Result<TaskContextView> {
        let context = self.load_or_default(task_id, "")?;
        let used = UnitStore::new(self.db).total_model_calls(task_id)?;
        let remaining = if max_model_calls == 0 {
            None
        } else {
            Some(max_model_calls.saturating_sub(used))
        };
        Ok(TaskContextView {
            context,
            model_calls_used: used,
            remaining_budget: remaining,
        })
    }

    /// Idempotent adds: a fact, constraint, evidence ref or open item
    /// already present (same text / same kind+id) is not duplicated,
    /// so a retried worker cannot grow the context by re-recording.
    pub fn add_fact(&self, task_id: &str, fact: &str, now: &Timestamp) -> Result<TaskContext> {
        let mut ctx = self.load_or_default(task_id, "")?;
        if !ctx.facts.iter().any(|f| f == fact) {
            ctx.facts.push(fact.to_string());
        }
        self.save(&ctx, now)?;
        Ok(ctx)
    }

    pub fn add_constraint(
        &self,
        task_id: &str,
        constraint: &str,
        now: &Timestamp,
    ) -> Result<TaskContext> {
        let mut ctx = self.load_or_default(task_id, "")?;
        if !ctx.constraints.iter().any(|c| c == constraint) {
            ctx.constraints.push(constraint.to_string());
        }
        self.save(&ctx, now)?;
        Ok(ctx)
    }

    pub fn add_evidence(
        &self,
        task_id: &str,
        evidence: &EvidenceRef,
        now: &Timestamp,
    ) -> Result<TaskContext> {
        let mut ctx = self.load_or_default(task_id, "")?;
        if !ctx
            .evidence
            .iter()
            .any(|e| e.kind == evidence.kind && e.id == evidence.id)
        {
            ctx.evidence.push(evidence.clone());
        }
        self.save(&ctx, now)?;
        Ok(ctx)
    }

    pub fn add_open_item(&self, task_id: &str, item: &str, now: &Timestamp) -> Result<TaskContext> {
        let mut ctx = self.load_or_default(task_id, "")?;
        if !ctx.open_items.iter().any(|i| i == item) {
            ctx.open_items.push(item.to_string());
        }
        self.save(&ctx, now)?;
        Ok(ctx)
    }

    /// Close an open item (remove it). Closing an item that is not
    /// open is a no-op, not an error.
    pub fn close_open_item(
        &self,
        task_id: &str,
        item: &str,
        now: &Timestamp,
    ) -> Result<TaskContext> {
        let mut ctx = self.load_or_default(task_id, "")?;
        ctx.open_items.retain(|i| i != item);
        self.save(&ctx, now)?;
        Ok(ctx)
    }

    /// Set the goal (the delivery goal the plan serves). The goal is
    /// part of the context so a replan that rewrites steps cannot
    /// silently drop what the task is for.
    pub fn set_goal(&self, task_id: &str, goal: &str, now: &Timestamp) -> Result<TaskContext> {
        let mut ctx = self.load_or_default(task_id, "")?;
        ctx.goal = goal.to_string();
        self.save(&ctx, now)?;
        Ok(ctx)
    }
}

/// A context row is only meaningful for a task that exists; saving
/// one for an unknown task id is a caller bug, caught here instead of
/// silently creating an orphan row.
pub fn check_task_exists(db: &Db, task_id: &str) -> Result<()> {
    let c = db.conn();
    let found: Option<String> = c
        .query_row(
            "SELECT id FROM tasks WHERE id = ?1",
            params![task_id],
            |r| r.get(0),
        )
        .optional()
        .db()?;
    match found {
        Some(_) => Ok(()),
        None => Err(Error::NotFound(format!("task {task_id} 不存在"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatch_store::Db;

    fn now() -> Timestamp {
        hatch_core::time::now()
    }

    #[test]
    fn save_load_roundtrip_and_dedupe() {
        let db = Db::open_in_memory().unwrap();
        let tasks = crate::TaskStore::new(&db);
        let task = tasks
            .create("test", "安排面试", "worker", "", 3, None, &now())
            .unwrap();
        let id = task.id.as_str();
        let store = TaskContextStore::new(&db);
        let ctx = TaskContext {
            task_id: id.into(),
            goal: "安排面试".into(),
            facts: vec!["候选人：李婷".into()],
            constraints: vec!["不得向未确认地址发邮件".into()],
            evidence: vec![EvidenceRef::new("mail", "m1", "李婷的回复")],
            open_items: vec!["等王宇回复".into()],
        };
        store.save(&ctx, &now()).unwrap();
        let loaded = store.load(id).unwrap().unwrap();
        assert_eq!(loaded, ctx);
        // Idempotent adds: a second identical add changes nothing.
        store.add_fact(id, "候选人：李婷", &now()).unwrap();
        store
            .add_evidence(id, &EvidenceRef::new("mail", "m1", "李婷的回复"), &now())
            .unwrap();
        let loaded = store.load(id).unwrap().unwrap();
        assert_eq!(loaded.facts.len(), 1);
        assert_eq!(loaded.evidence.len(), 1);
        // Close the open item.
        store.close_open_item(id, "等王宇回复", &now()).unwrap();
        assert!(store.load(id).unwrap().unwrap().open_items.is_empty());
        // Unknown task: no context.
        assert!(store.load("tsk_missing").unwrap().is_none());
        // Saving for an unknown task is refused (no orphan rows).
        let orphan = TaskContext {
            task_id: "tsk_missing".into(),
            goal: "x".into(),
            ..Default::default()
        };
        assert!(store.save(&orphan, &now()).is_err());
    }

    #[test]
    fn remaining_budget_comes_from_the_ledger() {
        let db = Db::open_in_memory().unwrap();
        let store = TaskContextStore::new(&db);
        // No budget cap: unlimited.
        let view = store.view("tsk_y", 0).unwrap();
        assert_eq!(view.remaining_budget, None);
        assert_eq!(view.model_calls_used, 0);
        // A cap with no calls yet: the full remainder.
        let view = store.view("tsk_y", 10).unwrap();
        assert_eq!(view.remaining_budget, Some(10));
    }
}
