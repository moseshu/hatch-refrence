//! Context compaction as a background job.
//!
//! The main turn loop never waits for a model summary any more: when the
//! live history passes the compaction threshold, [`request_compaction`]
//! enqueues a one-shot `compaction` job (kind [`KIND`]) and the turn
//! continues with the full history. The job is owned by the scheduler —
//! the daemon runs it on its next tick — and `hatch chat` drains a due
//! job for its own session in a detached thread when the daemon is not
//! running, so a queued job is never left pending forever. The next turn
//! reads the summary through `SessionStore::active`, unchanged.
//!
//! The worker lives here (not in `hatch-workers`) because the pieces it
//! needs — [`compaction`](crate::compaction) and [`SessionStore`] —
//! already live in `hatch-agent`, and `hatch-scheduler` (its only new
//! dependency) does not depend on `hatch-agent`, so no cycle is
//! introduced. Extracting the compaction logic to a shared crate would
//! have touched more files for no gain.

use crate::compaction;
use crate::session::SessionStore;
use chrono_tz::Tz;
use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use hatch_llm::Router;
use hatch_scheduler::{JobContext, JobHandler, JobOutcome, JobStore, NewJob, RunStatus};
use hatch_store::Db;
use serde_json::json;
use std::sync::Arc;

/// Job kind for one-shot session compactions.
pub const KIND: &str = "compaction";

/// Builds a fresh router (fresh call budget) for a compaction run, or
/// `None` when no model is configured. Same shape as the workers'
/// `RouterFactory`; a separate alias so `hatch-agent` does not depend on
/// `hatch-workers`.
pub type RouterFactory = Arc<dyn Fn() -> Result<Router> + Send + Sync>;

/// One compaction job per session; the name is the dedup key.
pub fn job_name(session: &str) -> String {
    format!("compaction:{session}")
}

/// Enqueue a one-shot compaction of `session` up to `upto_seq`.
///
/// Returns `true` when a job was newly enqueued. At most one pending or
/// running compaction job exists per session: if one is already queued
/// the call is a no-op returning `false`. A finished (or disabled)
/// previous job is removed and replaced, so a later request can fire
/// again; run history is kept by the store.
pub fn request_compaction(
    db: &Db,
    session: &str,
    upto_seq: i64,
    now: &Timestamp,
    tz: &Tz,
) -> Result<bool> {
    let store = JobStore::new(db);
    let name = job_name(session);
    if let Some(job) = store.find(&name)? {
        if job.enabled && job.next_run_at.is_some() {
            // Still pending (or running, or awaiting retry): do not pile up.
            return Ok(false);
        }
        store.remove(&job.id)?;
    }
    let at = to_rfc3339(&(*now + chrono::Duration::seconds(1)));
    let mut job = NewJob::new(&name, KIND, &format!("once {at}"));
    job.params = json!({ "session": session, "upto_seq": upto_seq });
    job.delivery = hatch_scheduler::Delivery::Silent;
    job.done_condition = "会话上下文已压缩，摘要已写入，下回合可见".into();
    job.timeout_secs = 300;
    store.add(&job, now, tz)?;
    Ok(true)
}

/// Is there a compaction job for `session` that is due right now and not
/// already running? Returns its name. Used by the chat drain: the daemon
/// owns the job when it runs, but a chat process must not leave it
/// pending forever when the daemon is down.
pub fn compaction_due(db: &Db, session: &str, now: &Timestamp) -> Result<Option<String>> {
    let store = JobStore::new(db);
    let Some(job) = store.find(&job_name(session))? else {
        return Ok(None);
    };
    if !job.enabled || job.kind != KIND {
        return Ok(None);
    }
    let Some(next) = job.next_run_at else {
        return Ok(None);
    };
    if next > *now {
        return Ok(None);
    }
    if let Some(latest) = store.history(Some(&job.id), 1)?.into_iter().next() {
        if latest.status == RunStatus::Running {
            return Ok(None);
        }
    }
    Ok(Some(job.name))
}

/// The background worker: summarise `seq <= upto_seq` and store the
/// summary for the next turn. Idempotent — rows already compacted are
/// skipped, and messages appended after the request stay live. When no
/// model is configured (or the router cannot be built) it falls back to
/// the extractive summary instead of failing, so the session still gets
/// compacted.
pub struct CompactionWorker {
    pub router: Option<RouterFactory>,
}

impl JobHandler for CompactionWorker {
    fn run(&self, ctx: &JobContext) -> Result<JobOutcome> {
        let session = ctx
            .job
            .params
            .get("session")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Invalid("compaction job needs a session param".into()))?;
        let upto = ctx
            .job
            .params
            .get("upto_seq")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| Error::Invalid("compaction job needs an upto_seq param".into()))?;
        let sessions = SessionStore::new(ctx.db);
        let (summary, history) = sessions.active(session)?;
        // Live messages are ordered by seq and compaction only ever marks
        // a prefix compacted, so the rows to summarise are a prefix.
        let n = history.iter().take_while(|m| m.seq <= upto).count();
        if n == 0 {
            return Ok(JobOutcome::Skipped {
                reason: format!("会话 {session} 第 {upto} 条及以前的消息已压缩过"),
            });
        }
        let msgs = &history[..n];
        let (text, used_model) = match &self.router {
            Some(factory) => match factory() {
                Ok(router) => compaction::summarize(&router, summary.as_deref(), msgs),
                Err(_) => (compaction::extractive(summary.as_deref(), msgs), false),
            },
            None => (compaction::extractive(summary.as_deref(), msgs), false),
        };
        sessions.compact(session, upto, &text, &ctx.now)?;
        Ok(JobOutcome::Done {
            summary: format!(
                "已压缩 {n} 条消息（到第 {upto} 条），摘要 {} 字{}",
                text.chars().count(),
                if used_model {
                    "，模型摘要"
                } else {
                    "，抽取式"
                }
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatch_core::time::now;
    use hatch_core::{CancelToken, Trust};
    use hatch_llm::scripted::ScriptedModel;
    use hatch_llm::{ChatMessage, ChatResponse};
    use hatch_scheduler::Trigger;
    use hatch_store::params;
    use tempfile::TempDir;

    fn test_db() -> (TempDir, Db) {
        let dir = TempDir::new().unwrap();
        let db = Db::open(&dir.path().join("hatch.db")).unwrap();
        (dir, db)
    }

    fn fill_session(db: &Db, n_user_turns: usize) -> (String, Vec<i64>) {
        let sessions = SessionStore::new(db);
        let now = now();
        let id = sessions.create("test", &now).unwrap();
        let mut seqs = Vec::new();
        for i in 0..n_user_turns {
            seqs.push(
                sessions
                    .append(
                        &id,
                        &ChatMessage::user(format!("用户消息 {i} ")),
                        Trust::User,
                        &now,
                    )
                    .unwrap(),
            );
            sessions
                .append(
                    &id,
                    &ChatMessage::assistant(format!("助手回复 {i}")),
                    Trust::System,
                    &now,
                )
                .unwrap();
        }
        (id, seqs)
    }

    /// Run the handler the way the engine would: a run row is opened
    /// first and closed with the outcome, so `job_runs` history is real.
    fn run_worker(db: &Db, job_name: &str, router: Option<RouterFactory>) -> JobOutcome {
        let store = JobStore::new(db);
        let job = store.get(job_name).unwrap();
        let run_id = store.start_run(&job.id, "manual", None, &now()).unwrap();
        let ctx = JobContext {
            job: &job,
            db,
            now: now(),
            tz: chrono_tz::Asia::Shanghai,
            cancel: CancelToken::new(),
            trigger: Trigger::Manual,
            run_id: run_id.clone(),
        };
        let outcome = CompactionWorker { router }.run(&ctx);
        let (status, summary) = match &outcome {
            Ok(JobOutcome::Done { summary }) => (RunStatus::Ok, summary.clone()),
            Ok(JobOutcome::Skipped { reason }) => (RunStatus::Skipped, reason.clone()),
            Err(e) => (RunStatus::Failed, e.to_string()),
        };
        store
            .finish_run(&run_id, status, &summary, "", &now())
            .unwrap();
        outcome.unwrap()
    }

    #[test]
    fn request_dedupes_per_session() {
        let (_dir, db) = test_db();
        let (id, _) = fill_session(&db, 3);
        let now = now();
        let tz = chrono_tz::Asia::Shanghai;
        assert!(request_compaction(&db, &id, 4, &now, &tz).unwrap());
        assert!(!request_compaction(&db, &id, 4, &now, &tz).unwrap());
        assert!(!request_compaction(&db, &id, 5, &now, &tz).unwrap());
        assert_eq!(JobStore::new(&db).list().unwrap().len(), 1);
    }

    #[test]
    fn request_rearms_after_completion_and_keeps_history() {
        let (_dir, db) = test_db();
        let (id, seqs) = fill_session(&db, 3);
        let now = now();
        let tz = chrono_tz::Asia::Shanghai;
        assert!(request_compaction(&db, &id, seqs[1], &now, &tz).unwrap());
        let name = job_name(&id);
        assert!(matches!(
            run_worker(&db, &name, None),
            JobOutcome::Done { .. }
        ));
        // Simulate what the engine does after a run: a once-job has no
        // next firing, so it is dormant. A new request then replaces it,
        // and the old run stays in history under the old job id.
        let old_id = JobStore::new(&db).get(&name).unwrap().id;
        JobStore::new(&db)
            .set_state(&old_id, None, None, 0, Some(now), "ok")
            .unwrap();
        assert!(request_compaction(&db, &id, seqs[2], &now, &tz).unwrap());
        assert_eq!(JobStore::new(&db).list().unwrap().len(), 1);
        assert_eq!(
            JobStore::new(&db).history(Some(&old_id), 10).unwrap().len(),
            1
        );
    }

    #[test]
    fn worker_writes_summary_and_keeps_later_messages_live() {
        let (_dir, db) = test_db();
        let (id, seqs) = fill_session(&db, 4);
        let now = now();
        let tz = chrono_tz::Asia::Shanghai;
        // Compact up to the 2nd user turn; its assistant reply and the
        // 3rd/4th turns were appended after the boundary and must stay live.
        assert!(request_compaction(&db, &id, seqs[1], &now, &tz).unwrap());
        let name = job_name(&id);
        assert!(matches!(
            run_worker(&db, &name, None),
            JobOutcome::Done { .. }
        ));
        let (summary, live) = SessionStore::new(&db).active(&id).unwrap();
        assert!(summary.is_some(), "next turn must see the summary");
        assert_eq!(
            live.len(),
            5,
            "reply to the compacted turn + two later turns stay live"
        );
        assert!(live.iter().all(|m| m.seq > seqs[1]));
    }

    #[test]
    fn worker_is_idempotent() {
        let (_dir, db) = test_db();
        let (id, seqs) = fill_session(&db, 3);
        let now = now();
        let tz = chrono_tz::Asia::Shanghai;
        assert!(request_compaction(&db, &id, seqs[1], &now, &tz).unwrap());
        let name = job_name(&id);
        assert!(matches!(
            run_worker(&db, &name, None),
            JobOutcome::Done { .. }
        ));
        assert!(matches!(
            run_worker(&db, &name, None),
            JobOutcome::Skipped { .. }
        ));
        // Still exactly one summary row: the second run wrote nothing.
        let (summary, _) = SessionStore::new(&db).active(&id).unwrap();
        assert!(summary.is_some());
    }

    #[test]
    fn worker_uses_model_when_configured() {
        let (_dir, db) = test_db();
        let (id, seqs) = fill_session(&db, 2);
        let now = now();
        let tz = chrono_tz::Asia::Shanghai;
        assert!(request_compaction(&db, &id, seqs[0], &now, &tz).unwrap());
        let model = Arc::new(ScriptedModel::ok(
            "c",
            vec![ChatResponse::text("模型写的摘要：用户问了两件事")],
        ));
        let factory: RouterFactory = Arc::new(move || Ok(Router::single(model.clone())));
        let name = job_name(&id);
        let outcome = run_worker(&db, &name, Some(factory));
        assert!(matches!(outcome, JobOutcome::Done { summary } if summary.contains("模型摘要")));
        let (summary, _) = SessionStore::new(&db).active(&id).unwrap();
        assert!(summary.unwrap().contains("模型写的摘要"));
    }

    #[test]
    fn compaction_due_decisions() {
        let (_dir, db) = test_db();
        let (id, _) = fill_session(&db, 2);
        let now = now();
        let tz = chrono_tz::Asia::Shanghai;
        assert_eq!(compaction_due(&db, &id, &now).unwrap(), None);
        assert!(request_compaction(&db, &id, 2, &now, &tz).unwrap());
        // Fires 1s in the future: not due yet.
        assert_eq!(compaction_due(&db, &id, &now).unwrap(), None);
        // Move the firing into the past: due.
        let name = job_name(&id);
        let past = to_rfc3339(&(now - chrono::Duration::seconds(10)));
        db.conn()
            .execute(
                "UPDATE jobs SET next_run_at = ?1 WHERE name = ?2",
                params![past, name],
            )
            .unwrap();
        assert_eq!(compaction_due(&db, &id, &now).unwrap(), Some(name.clone()));
        // A run currently in flight: not due (the daemon already has it).
        let job = JobStore::new(&db).get(&name).unwrap();
        JobStore::new(&db)
            .start_run(&job.id, "manual", None, &now)
            .unwrap();
        assert_eq!(compaction_due(&db, &id, &now).unwrap(), None);
    }
}
