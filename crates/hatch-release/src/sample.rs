//! Eval samples, maintained by attribution class (task 9).
//!
//! A regression becomes a sample only after its trace is
//! desensitized — `record` refuses anything not marked as such.
//! The boundary that scrubs mail and addresses before they reach
//! a model (task M2) is the same boundary a trace must cross
//! before it lands here; this store is the second lock, not the
//! first. Samples are queryable by class, so a regression review
//! starts from "which class is failing" instead of from a log.

use hatch_core::ids;
use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use hatch_store::{params, Db, DbResultExt};
use rusqlite::Row;

use crate::attribution::{FailureClass, Stage};

/// One desensitized failure trace, kept as an eval sample.
#[derive(Clone, Debug)]
pub struct EvalSample {
    pub id: String,
    pub world: String,
    pub class: FailureClass,
    pub stage: Stage,
    pub task_id: String,
    pub step_id: String,
    /// Short desensitized summary: what failed, at which step,
    /// with which evidence. Never raw mail, addresses or secrets.
    pub summary: String,
    pub desensitized: bool,
    pub created_at: String,
}

const COLS: &str =
    "id, world, class, stage, task_id, step_id, summary, desensitized, created_at";

fn row(r: &Row) -> rusqlite::Result<EvalSample> {
    let class: String = r.get(2)?;
    let stage: String = r.get(3)?;
    Ok(EvalSample {
        id: r.get(0)?,
        world: r.get(1)?,
        class: FailureClass::parse(&class).unwrap_or(FailureClass::Runtime),
        stage: Stage::parse(&stage).unwrap_or(Stage::Execute),
        task_id: r.get(4)?,
        step_id: r.get(5)?,
        summary: r.get(6)?,
        desensitized: r.get::<_, i64>(7)? != 0,
        created_at: r.get(8)?,
    })
}

pub struct SampleStore<'a> {
    db: &'a Db,
}

impl<'a> SampleStore<'a> {
    pub fn new(db: &'a Db) -> Self {
        SampleStore { db }
    }

    /// Record a sample. Refuses anything not marked desensitized:
    /// an unscrubbed trace must never become a standing sample.
    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &self,
        world: &str,
        class: FailureClass,
        stage: Stage,
        task_id: &str,
        step_id: &str,
        summary: &str,
        desensitized: bool,
        now: &Timestamp,
    ) -> Result<EvalSample> {
        if !desensitized {
            return Err(Error::Invalid(
                "评估样本只收已脱敏的失败轨迹：先脱敏再收录".into(),
            ));
        }
        let id = ids::random_id("smp");
        let ts = to_rfc3339(now);
        let c = self.db.conn();
        c.execute(
            &format!(
                "INSERT INTO eval_samples({COLS}) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)"
            ),
            params![
                id,
                world,
                class.as_str(),
                stage.as_str(),
                task_id,
                step_id,
                summary,
                1i64,
                ts
            ],
        )
        .db()?;
        Ok(EvalSample {
            id,
            world: world.into(),
            class,
            stage,
            task_id: task_id.into(),
            step_id: step_id.into(),
            summary: summary.into(),
            desensitized: true,
            created_at: ts,
        })
    }

    /// Samples of one class, newest first.
    pub fn list_by_class(&self, class: FailureClass) -> Result<Vec<EvalSample>> {
        let c = self.db.conn();
        let mut st = c.prepare(&format!(
            "SELECT {COLS} FROM eval_samples WHERE class = ?1 ORDER BY created_at DESC, id DESC"
        )).db()?;
        let rows = st.query_map(params![class.as_str()], row).db()?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.db()?);
        }
        Ok(out)
    }

    /// All samples, newest first (bounded: samples are a review
    /// queue, not a log).
    pub fn list_all(&self, limit: usize) -> Result<Vec<EvalSample>> {
        let c = self.db.conn();
        let mut st = c.prepare(&format!(
            "SELECT {COLS} FROM eval_samples ORDER BY created_at DESC, id DESC LIMIT ?1"
        )).db()?;
        let rows = st.query_map(params![limit as i64], row).db()?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.db()?);
        }
        Ok(out)
    }

    /// Sample counts per class, in the plan's class order.
    pub fn count_by_class(&self) -> Result<Vec<(FailureClass, i64)>> {
        let c = self.db.conn();
        let mut st = c
            .prepare("SELECT class, COUNT(*) FROM eval_samples GROUP BY class")
            .db()?;
        let rows = st
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .db()?;
        let mut by_class: std::collections::HashMap<String, i64> =
            std::collections::HashMap::new();
        for r in rows {
            let (class, n) = r.db()?;
            by_class.insert(class, n);
        }
        Ok(FailureClass::all()
            .iter()
            .map(|c| (*c, by_class.get(c.as_str()).copied().unwrap_or(0)))
            .collect())
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
    fn unscrubbed_trace_is_refused() {
        let db = Db::open_in_memory().unwrap();
        let store = SampleStore::new(&db);
        let err = store
            .record(
                "meeting-brief",
                FailureClass::Permission,
                Stage::Approve,
                "tsk_x",
                "s1",
                "审批被拒",
                false,
                &now(),
            )
            .unwrap_err();
        assert!(err.to_string().contains("脱敏"));
        assert!(store.list_all(10).unwrap().is_empty());
    }

    #[test]
    fn record_and_list_by_class() {
        let db = Db::open_in_memory().unwrap();
        let store = SampleStore::new(&db);
        store
            .record(
                "meeting-brief",
                FailureClass::Tool,
                Stage::Execute,
                "tsk_x",
                "s1",
                "工具超时（已脱敏）",
                true,
                &now(),
            )
            .unwrap();
        store
            .record(
                "meeting-brief",
                FailureClass::Tool,
                Stage::Execute,
                "tsk_y",
                "s1",
                "工具未知（已脱敏）",
                true,
                &now(),
            )
            .unwrap();
        assert_eq!(store.list_by_class(FailureClass::Tool).unwrap().len(), 2);
        assert_eq!(
            store.list_by_class(FailureClass::Model).unwrap().len(),
            0
        );
        let counts = store.count_by_class().unwrap();
        assert_eq!(
            counts
                .iter()
                .find(|(c, _)| *c == FailureClass::Tool)
                .unwrap()
                .1,
            2
        );
        assert_eq!(counts.len(), 6);
    }
}
