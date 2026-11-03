//! Persistence for sub-agent runs and their progress events.

use hatch_core::time::{now, to_rfc3339, Timestamp};
use hatch_core::Result;
use hatch_store::{params, Db, DbResultExt, OptionalExtension};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunStatus {
    Spawned,
    Running,
    Completed,
    Failed,
    Cancelled,
    /// Refused before starting (depth limit, unknown type).
    Rejected,
}

impl RunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunStatus::Spawned => "spawned",
            RunStatus::Running => "running",
            RunStatus::Completed => "completed",
            RunStatus::Failed => "failed",
            RunStatus::Cancelled => "cancelled",
            RunStatus::Rejected => "rejected",
        }
    }

    pub fn parse(s: &str) -> RunStatus {
        match s {
            "spawned" => RunStatus::Spawned,
            "running" => RunStatus::Running,
            "completed" => RunStatus::Completed,
            "cancelled" => RunStatus::Cancelled,
            "rejected" => RunStatus::Rejected,
            _ => RunStatus::Failed,
        }
    }

    pub fn is_terminal(&self) -> bool {
        !matches!(self, RunStatus::Spawned | RunStatus::Running)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunRecord {
    pub id: String,
    pub parent: String,
    pub session: String,
    pub child_session: String,
    pub depth: usize,
    pub agent_type: String,
    pub task: String,
    pub status: RunStatus,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub steps: usize,
    pub answer: String,
    pub error: String,
    pub model_calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunEvent {
    pub seq: i64,
    pub ts: String,
    pub kind: String,
    pub detail: String,
}

/// Final numbers written when a run ends.
#[derive(Clone, Debug, Default)]
pub struct RunResult {
    pub steps: usize,
    pub answer: String,
    pub error: String,
    pub model_calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

pub struct RunStore<'a> {
    db: &'a Db,
}

const COLS: &str = "id, parent, session, child_session, depth, agent_type, task, status, started_at, finished_at, steps, answer, error, model_calls, prompt_tokens, completion_tokens";

fn row(r: &hatch_store::Row) -> hatch_store::rusqlite::Result<RunRecord> {
    Ok(RunRecord {
        id: r.get(0)?,
        parent: r.get(1)?,
        session: r.get(2)?,
        child_session: r.get(3)?,
        depth: r.get::<_, i64>(4)? as usize,
        agent_type: r.get(5)?,
        task: r.get(6)?,
        status: RunStatus::parse(&r.get::<_, String>(7)?),
        started_at: r.get(8)?,
        finished_at: r.get(9)?,
        steps: r.get::<_, i64>(10)? as usize,
        answer: r.get(11)?,
        error: r.get(12)?,
        model_calls: r.get::<_, i64>(13)? as u64,
        prompt_tokens: r.get::<_, i64>(14)? as u64,
        completion_tokens: r.get::<_, i64>(15)? as u64,
    })
}

impl<'a> RunStore<'a> {
    pub fn new(db: &'a Db) -> Self {
        RunStore { db }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create(&self, id: &str, parent: &str, session: &str, depth: usize, agent_type: &str, task: &str, status: RunStatus, ts: &Timestamp) -> Result<()> {
        self.db
            .conn()
            .execute(
                "INSERT INTO subagent_runs(id, parent, session, depth, agent_type, task, status, started_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![id, parent, session, depth as i64, agent_type, hatch_core::secrets::redact(task), status.as_str(), to_rfc3339(ts)],
            )
            .db()?;
        Ok(())
    }

    pub fn start(&self, id: &str, child_session: &str) -> Result<()> {
        self.db
            .conn()
            .execute("UPDATE subagent_runs SET status = 'running', child_session = ?2 WHERE id = ?1", params![id, child_session])
            .db()?;
        Ok(())
    }

    pub fn finish(&self, id: &str, status: RunStatus, r: &RunResult) -> Result<()> {
        self.db
            .conn()
            .execute(
                "UPDATE subagent_runs SET status = ?2, finished_at = ?3, steps = ?4, answer = ?5, error = ?6,
                 model_calls = ?7, prompt_tokens = ?8, completion_tokens = ?9 WHERE id = ?1",
                params![
                    id,
                    status.as_str(),
                    to_rfc3339(&now()),
                    r.steps as i64,
                    hatch_core::secrets::redact(&r.answer),
                    hatch_core::secrets::redact(&r.error),
                    r.model_calls as i64,
                    r.prompt_tokens as i64,
                    r.completion_tokens as i64
                ],
            )
            .db()?;
        Ok(())
    }

    /// Append a progress line (already short; redacted again here).
    pub fn event(&self, id: &str, kind: &str, detail: &str) -> Result<()> {
        self.db.transaction(|c| {
            let seq: i64 = c
                .query_row("SELECT COALESCE(MAX(seq), 0) + 1 FROM subagent_events WHERE run_id = ?1", params![id], |r| r.get(0))
                .db()?;
            c.execute(
                "INSERT INTO subagent_events(run_id, seq, ts, kind, detail) VALUES(?1,?2,?3,?4,?5)",
                params![id, seq, to_rfc3339(&now()), kind, hatch_core::secrets::redact(detail)],
            )
            .db()?;
            Ok(())
        })
    }

    pub fn get(&self, id: &str) -> Result<Option<RunRecord>> {
        self.db
            .conn()
            .query_row(&format!("SELECT {COLS} FROM subagent_runs WHERE id = ?1"), params![id], row)
            .optional()
            .db()
    }

    /// Runs newest first, optionally only those of one top-level session.
    pub fn list(&self, session: Option<&str>, limit: usize) -> Result<Vec<RunRecord>> {
        let conn = self.db.conn();
        let mut st = conn
            .prepare(&format!(
                "SELECT {COLS} FROM subagent_runs WHERE (?1 IS NULL OR session = ?1) ORDER BY started_at DESC, rowid DESC LIMIT ?2"
            ))
            .db()?;
        let rows = st.query_map(params![session, limit as i64], row).db()?;
        rows.collect::<hatch_store::rusqlite::Result<Vec<_>>>().db()
    }

    pub fn events(&self, id: &str) -> Result<Vec<RunEvent>> {
        let conn = self.db.conn();
        let mut st = conn.prepare("SELECT seq, ts, kind, detail FROM subagent_events WHERE run_id = ?1 ORDER BY seq").db()?;
        let rows = st
            .query_map(params![id], |r| Ok(RunEvent { seq: r.get(0)?, ts: r.get(1)?, kind: r.get(2)?, detail: r.get(3)? }))
            .db()?;
        rows.collect::<hatch_store::rusqlite::Result<Vec<_>>>().db()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_and_events() {
        let db = Db::open_in_memory().unwrap();
        let s = RunStore::new(&db);
        let t = now();
        s.create("sa_1", "session:s", "s", 1, "explore", "数一数文件，密码：hunter22x", RunStatus::Spawned, &t).unwrap();
        s.start("sa_1", "ses_child").unwrap();
        s.event("sa_1", "tool_call", "fs_list path=.").unwrap();
        s.event("sa_1", "tool_result", "ok").unwrap();
        s.finish("sa_1", RunStatus::Completed, &RunResult { steps: 2, answer: "3 个文件".into(), model_calls: 2, ..Default::default() }).unwrap();
        let r = s.get("sa_1").unwrap().unwrap();
        assert_eq!(r.status, RunStatus::Completed);
        assert_eq!(r.child_session, "ses_child");
        assert!(!r.task.contains("hunter22x"));
        assert_eq!(s.events("sa_1").unwrap().iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(s.list(Some("s"), 10).unwrap().len(), 1);
        assert_eq!(s.list(Some("other"), 10).unwrap().len(), 0);
        assert!(RunStatus::Cancelled.is_terminal() && !RunStatus::Running.is_terminal());
    }
}
