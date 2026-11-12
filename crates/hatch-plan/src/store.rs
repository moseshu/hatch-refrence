//! [`PlanStore`]: plans in SQLite, next to approvals and sessions.
//!
//! A plan is one row in `plans` plus one row per step in `plan_steps`
//! (step rows carry their execution status and output). The store owns the
//! state machine: every transition checks the current status first, so an
//! approved plan cannot be silently re-edited into a different plan —
//! editing an approved plan drops it back to `pending_approval` and clears
//! the approved snapshot, and approving snapshots the exact steps approved.

use crate::plan::{Milestone, Plan, PlanStatus, PlanStep, PlanSummary, RiskLevel, StepStatus};
use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::{ids, Error, Result};
use hatch_store::{params, Db, DbResultExt, OptionalExtension};
use serde_json::Value;

/// One row of `plans` (15 columns).
type PlanRow = (
    String,
    String,
    String,
    String,
    // M1: plan revision (§10.2).
    i64,
    String,
    String,
    String,
    Option<String>,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
);

pub struct PlanStore<'a> {
    pub(crate) db: &'a Db,
}

fn row_to_step(r: &hatch_store::Row) -> hatch_store::rusqlite::Result<PlanStep> {
    let arguments: String = r.get(5)?;
    let files: String = r.get(6)?;
    let risk: String = r.get(8)?;
    let status: String = r.get(9)?;
    let approval_id: String = r.get(11)?;
    Ok(PlanStep {
        id: r.get(2)?,
        title: r.get(3)?,
        tool: r.get(4)?,
        arguments: serde_json::from_str(&arguments).unwrap_or(Value::Null),
        files: serde_json::from_str(&files).unwrap_or_default(),
        expected: r.get(7)?,
        risk: RiskLevel::parse(&risk).unwrap_or_default(),
        status: StepStatus::parse(&status).unwrap_or(StepStatus::Pending),
        output: r.get(10)?,
        approval_id: if approval_id.is_empty() {
            None
        } else {
            Some(approval_id)
        },
    })
}

const STEP_COLS: &str = "plan_id, idx, step_id, title, tool, arguments, files, expected, risk, status, output, approval_id, updated_at";

impl<'a> PlanStore<'a> {
    pub fn new(db: &'a Db) -> Self {
        PlanStore { db }
    }

    fn steps_of(&self, plan_id: &str) -> Result<Vec<PlanStep>> {
        let c = self.db.conn();
        let mut st = c
            .prepare(&format!(
                "SELECT {STEP_COLS} FROM plan_steps WHERE plan_id = ?1 ORDER BY idx"
            ))
            .db()?;
        let rows = st.query_map(params![plan_id], row_to_step).db()?;
        rows.collect::<hatch_store::rusqlite::Result<Vec<_>>>().db()
    }

    fn insert_steps(&self, plan_id: &str, steps: &[PlanStep], now: &Timestamp) -> Result<()> {
        for (i, s) in steps.iter().enumerate() {
            self.db
                .conn()
                .execute(
                    "INSERT INTO plan_steps(plan_id, idx, step_id, title, tool, arguments, files, expected, risk, status, output, approval_id, updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                    params![
                        plan_id,
                        i as i64,
                        s.id,
                        s.title,
                        s.tool,
                        s.arguments.to_string(),
                        serde_json::to_string(&s.files).unwrap_or_default(),
                        s.expected,
                        s.risk.as_str(),
                        s.status.as_str(),
                        s.output,
                        s.approval_id.as_deref().unwrap_or(""),
                        to_rfc3339(now),
                    ],
                )
                .db()?;
        }
        Ok(())
    }

    /// Create a plan in `pending_approval` (or `draft` when `draft` is set).
    /// Plans for the identical task that are still draft / pending are
    /// superseded — a newer plan replaces, never silently merges with, an
    /// older unapproved one. A non-draft plan submits its approval to the
    /// approval queue ([`crate::approval::submit_plan_approval`]); a draft
    /// submits nothing. Superseded plans' pending approvals are rejected,
    /// so the queue never offers a decision for a plan that can no longer
    /// run. `goal` is the durable goal the plan serves (may be empty);
    /// `task` stays the prompt that produced the plan.
    pub fn create(
        &self,
        task: &str,
        goal: &str,
        model: &str,
        steps: Vec<PlanStep>,
        draft: bool,
        now: &Timestamp,
    ) -> Result<Plan> {
        let id = ids::random_id("plan");
        let status = if draft {
            PlanStatus::Draft
        } else {
            PlanStatus::PendingApproval
        };
        let superseded: Vec<String> = self
            .db
            .conn()
            .prepare(
                "SELECT id FROM plans WHERE task = ?1 AND status IN ('draft', 'pending_approval')",
            )
            .db()?
            .query_map(params![task], |r| r.get(0))
            .db()?
            .collect::<hatch_store::rusqlite::Result<Vec<String>>>()
            .db()?;
        self.db.transaction(|c| {
            c.execute(
                "UPDATE plans SET status = 'superseded', updated_at = ?2 WHERE task = ?1 AND status IN ('draft', 'pending_approval')",
                params![task, to_rfc3339(now)],
            )
            .db()?;
            c.execute(
                "INSERT INTO plans(id, task, goal, status, model, created_at, updated_at, current_step, milestones_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![id, task, goal, status.as_str(), model, to_rfc3339(now), to_rfc3339(now), "", "[]"],
            )
            .db()?;
            Ok(())
        })?;
        self.insert_steps(&id, &steps, now)?;
        let plan = self
            .get(&id)?
            .ok_or_else(|| Error::Storage(format!("plan {id} vanished after create")))?;
        for old in &superseded {
            if *old != id {
                crate::approval::reject_pending_plan_approvals(self.db, old, now);
            }
        }
        if !draft {
            crate::approval::submit_plan_approval(self.db, &plan, now)?;
        }
        Ok(plan)
    }

    /// Set the durable goal a plan serves. The goal is independent of
    /// `task` (the prompt that produced the plan); changing it never
    /// touches the approval snapshot, so no re-approval is needed.
    pub fn set_goal(&self, id: &str, goal: &str, now: &Timestamp) -> Result<Plan> {
        self.get(id)?
            .ok_or_else(|| Error::NotFound(format!("plan {id} 不存在")))?;
        self.db
            .conn()
            .execute(
                "UPDATE plans SET goal = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, goal, to_rfc3339(now)],
            )
            .db()?;
        Ok(self.get(id)?.unwrap())
    }

    /// Replace the whole milestone list (written with the plan as one
    /// JSON array; no separate table).
    pub fn set_milestones(
        &self,
        id: &str,
        milestones: &[Milestone],
        now: &Timestamp,
    ) -> Result<Plan> {
        self.get(id)?
            .ok_or_else(|| Error::NotFound(format!("plan {id} 不存在")))?;
        self.db
            .conn()
            .execute(
                "UPDATE plans SET milestones_json = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, Plan::milestones_json(milestones), to_rfc3339(now)],
            )
            .db()?;
        Ok(self.get(id)?.unwrap())
    }

    /// Add a milestone by title, or flip the `done` flag of the one
    /// already there. Titles identify milestones within a plan.
    pub fn upsert_milestone(
        &self,
        id: &str,
        title: &str,
        done: bool,
        now: &Timestamp,
    ) -> Result<Plan> {
        let plan = self
            .get(id)?
            .ok_or_else(|| Error::NotFound(format!("plan {id} 不存在")))?;
        let mut milestones = plan.milestones;
        match milestones.iter_mut().find(|m| m.title == title) {
            Some(m) => m.done = done,
            None => milestones.push(Milestone {
                title: title.into(),
                done,
            }),
        }
        self.set_milestones(id, &milestones, now)
    }

    pub fn get(&self, id: &str) -> Result<Option<Plan>> {
        let row: Option<PlanRow> = self.db
                .conn()
                .query_row(
                    "SELECT id, task, goal, status, version, model, created_at, updated_at, decided_at, decided_by, decision_note, approved_steps_json, approved_hash, deviation, current_step, milestones_json FROM plans WHERE id = ?1",
                    params![id],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                            r.get(6)?,
                            r.get(7)?,
                            r.get(8)?,
                            r.get(9)?,
                            r.get(10)?,
                            r.get(11)?,
                            r.get(12)?,
                            r.get(13)?,
                            r.get(14)?,
                            r.get(15)?,
                        ))
                    },
                )
                .optional()
                .db()?;
        let Some((
            id,
            task,
            goal,
            status,
            version,
            model,
            created_at,
            updated_at,
            decided_at,
            decided_by,
            decision_note,
            approved_steps_json,
            approved_hash,
            deviation,
            current_step,
            milestones_json,
        )) = row
        else {
            return Ok(None);
        };
        Ok(Some(Plan {
            id: id.clone(),
            task,
            goal,
            status: PlanStatus::parse(&status)?,
            version: version as u64,
            model,
            steps: self.steps_of(&id)?,
            current_step: if current_step.is_empty() {
                None
            } else {
                Some(current_step)
            },
            milestones: Plan::parse_milestones_json(&milestones_json),
            created_at,
            updated_at,
            decided_at,
            decided_by,
            decision_note,
            approved_steps_json,
            approved_hash,
            deviation,
        }))
    }

    pub fn list(&self, status: Option<PlanStatus>, limit: usize) -> Result<Vec<PlanSummary>> {
        let c = self.db.conn();
        let mut out = Vec::new();
        let push = |r: &hatch_store::Row| -> hatch_store::rusqlite::Result<()> {
            let status: String = r.get(2)?;
            out.push(PlanSummary {
                id: r.get(0)?,
                task: r.get(1)?,
                status: PlanStatus::parse(&status).unwrap_or(PlanStatus::Draft),
                created_at: r.get(3)?,
                updated_at: r.get(4)?,
                steps: r.get::<_, i64>(5)? as usize,
            });
            Ok(())
        };
        match status {
            Some(s) => {
                let mut st = c
                    .prepare(
                        "SELECT p.id, p.task, p.status, p.created_at, p.updated_at, COUNT(s.idx) FROM plans p LEFT JOIN plan_steps s ON s.plan_id = p.id WHERE p.status = ?1 GROUP BY p.id ORDER BY p.created_at DESC LIMIT ?2",
                    )
                    .db()?;
                let rows = st.query_map(params![s.as_str(), limit as i64], push).db()?;
                rows.collect::<hatch_store::rusqlite::Result<Vec<_>>>()
                    .db()?;
            }
            None => {
                let mut st = c
                    .prepare(
                        "SELECT p.id, p.task, p.status, p.created_at, p.updated_at, COUNT(s.idx) FROM plans p LEFT JOIN plan_steps s ON s.plan_id = p.id GROUP BY p.id ORDER BY p.created_at DESC LIMIT ?1",
                    )
                    .db()?;
                let rows = st.query_map(params![limit as i64], push).db()?;
                rows.collect::<hatch_store::rusqlite::Result<Vec<_>>>()
                    .db()?;
            }
        }
        Ok(out)
    }
}
