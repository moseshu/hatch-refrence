//! The resume side of the unit record (M0): what a restarted worker
//! needs to pick up where the previous one died.
//!
//! A call is complete iff its result row exists, so [`UnitStore::pending_calls`]
//! is exactly the work a resumed worker must still run — a re-run
//! never re-executes a call that already produced a result, and a
//! step whose unit has no pending calls is done waiting for its
//! completion record. [`UnitStore::resume_position`] reports the first
//! unfinished step together with those calls.

use crate::unit::{ExecStatus, UnitCall, UnitCallSpec, UnitStore};
use hatch_core::Result;
use hatch_store::{params, DbResultExt};
use serde::{Deserialize, Serialize};

/// Where a crashed worker resumes: the first unfinished step and the
/// calls of its latest unit that still have no result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResumePosition {
    pub step_id: String,
    pub unit_seq: Option<i64>,
    pub pending_calls: Vec<UnitCallSpec>,
}

impl<'a> UnitStore<'a> {
    pub fn unit_results(&self, task_id: &str, unit_seq: i64) -> Result<Vec<UnitCall>> {
        let conn = self.db().conn();
        let mut st = conn
            .prepare(&format!(
                "SELECT {} FROM task_unit_results WHERE task_id = ?1 AND unit_seq = ?2 ORDER BY call_id",
                crate::unit::RESULT_COLS
            ))
            .db()?;
        let rows = st
            .query_map(params![task_id, unit_seq], crate::unit::result_row)
            .db()?;
        rows.collect::<hatch_store::rusqlite::Result<Vec<_>>>().db()
    }

    /// The calls of a unit that still have no result row: exactly what
    /// a resumed worker must still run. Empty means the unit is done.
    pub fn pending_calls(&self, task_id: &str, unit_seq: i64) -> Result<Vec<UnitCallSpec>> {
        let Some(unit) = self.get_unit(task_id, unit_seq)? else {
            return Ok(Vec::new());
        };
        let done: Vec<String> = self
            .unit_results(task_id, unit_seq)?
            .into_iter()
            .map(|r| r.call_id)
            .collect();
        Ok(unit
            .calls
            .into_iter()
            .filter(|c| !done.contains(&c.id))
            .collect())
    }

    /// Where a crashed worker resumes: the first step that is not done,
    /// with the pending calls of its latest unit (if any). `None` means
    /// every step is done.
    pub fn resume_position(&self, task_id: &str) -> Result<Option<ResumePosition>> {
        for step in self.list_steps(task_id)? {
            if step.status == ExecStatus::Done {
                continue;
            }
            let (unit_seq, pending) = match self.latest_unit(task_id, &step.step_id)? {
                Some(u) => (Some(u.unit_seq), self.pending_calls(task_id, u.unit_seq)?),
                None => (None, Vec::new()),
            };
            return Ok(Some(ResumePosition {
                step_id: step.step_id,
                unit_seq,
                pending_calls: pending,
            }));
        }
        Ok(None)
    }
}
