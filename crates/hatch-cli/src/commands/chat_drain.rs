//! Chat-time drain of due compaction jobs.
//!
//! The daemon owns compaction jobs when it is running; when it is not,
//! the chat process runs the pending job for its own session in a
//! detached thread so the summary lands for the next turn. If this
//! process exits first (e.g. one-shot `ask`), the job stays pending and
//! is picked up by the next turn or the daemon — the turn itself never
//! waits for a model summary.

use crate::app::App;

/// Run this session's due compaction job in a detached thread, if any.
/// Best effort: every failure mode just leaves the job pending.
pub(crate) fn drain_compaction(app: &App, session: &str) {
    let Ok(Some(name)) =
        hatch_agent::worker::compaction_due(&app.db, session, &hatch_core::time::now())
    else {
        return;
    };
    let home = app.home.clone();
    std::thread::spawn(move || {
        let now = hatch_core::time::now();
        let Ok(app) = App::open(Some(&home)) else {
            return;
        };
        let Ok(engine) = crate::sched::engine(&app) else {
            return;
        };
        let _ = engine.run_now(&name, &now, &hatch_core::CancelToken::new());
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatch_agent::worker::request_compaction;
    use hatch_agent::SessionStore;
    use hatch_core::Trust;
    use hatch_llm::ChatMessage;
    use hatch_scheduler::JobStore;
    use hatch_store::params;
    use std::time::{Duration, Instant};

    /// The real drain path: a due job is picked up by the detached thread
    /// and the summary lands, without the calling turn waiting for it.
    #[test]
    fn drain_runs_due_compaction_in_background() {
        // NOTE: do not touch HATCH_LLM_* here. Env vars are process-global
        // and tests run in parallel; another test sets them. A leaked
        // model config is harmless: the worker falls back to the
        // extractive summary when the model call fails.
        let dir = tempfile::tempdir().unwrap();
        let app = App::open(Some(dir.path())).unwrap();
        let now = hatch_core::time::now();
        let sessions = SessionStore::new(&app.db);
        let session = sessions.create("drain", &now).unwrap();
        sessions
            .append(&session, &ChatMessage::user("你好"), Trust::User, &now)
            .unwrap();
        sessions
            .append(
                &session,
                &ChatMessage::assistant("你好呀"),
                Trust::System,
                &now,
            )
            .unwrap();
        let upto = sessions
            .append(
                &session,
                &ChatMessage::user("再聊点别的"),
                Trust::User,
                &now,
            )
            .unwrap();
        assert!(request_compaction(&app.db, &session, upto, &now, &app.tz).unwrap());
        // Force the firing into the past so the job is due right now.
        let past = hatch_core::time::to_rfc3339(&(now - chrono::Duration::seconds(10)));
        app.db
            .conn()
            .execute(
                "UPDATE jobs SET next_run_at = ?1 WHERE name = ?2",
                params![past, hatch_agent::worker::job_name(&session)],
            )
            .unwrap();
        drain_compaction(&app, &session);
        // Poll for the run to finish (the thread opens its own App,
        // builds the engine and runs the job).
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let job = JobStore::new(&app.db)
                .get(&hatch_agent::worker::job_name(&session))
                .unwrap();
            let runs = JobStore::new(&app.db).history(Some(&job.id), 1).unwrap();
            if runs
                .iter()
                .any(|r| r.status == hatch_scheduler::RunStatus::Ok)
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "drain did not run the job in time"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        let (summary, _) = sessions.active(&session).unwrap();
        assert!(summary.is_some(), "summary must land after the drain");
    }
}
