//! Wake-turn tests (scripted model, zero cost).

use super::wake::{run_wake_turn, should_wake, WAKE_EVENT};
use crate::app::App;
use crate::config::AgentSection;
use hatch_agent::subagent::{RunStatus, RunStore};
use hatch_agent::SessionStore;
use hatch_core::CancelToken;
use hatch_llm::scripted::ScriptedModel;
use hatch_llm::{ChatResponse, Router};
use std::sync::Arc;

fn setup() -> (tempfile::TempDir, Arc<App>, String) {
    let dir = tempfile::tempdir().unwrap();
    let app = Arc::new(App::open(Some(dir.path())).unwrap());
    let session = SessionStore::new(&app.db)
        .create("test", &hatch_core::time::now())
        .unwrap();
    (dir, app, session)
}

/// A completed run of `session` with an undelivered handoff.
fn seed_handoff(app: &App, run_id: &str, session: &str, text: &str) {
    let now = hatch_core::time::now();
    let store = RunStore::new(&app.db);
    store
        .create(
            run_id,
            &format!("session:{session}"),
            session,
            1,
            "explore",
            "查预算",
            RunStatus::Completed,
            &now,
        )
        .unwrap();
    store.save_handoff(run_id, text, false, &now).unwrap();
}

fn scripted_router(answer: &str) -> Router {
    let model = Arc::new(ScriptedModel::ok(
        "test",
        vec![ChatResponse::text(answer)],
    ));
    Router::single(model)
}

#[test]
fn should_wake_matrix() {
    let (_dir, app, session) = setup();
    let now = hatch_core::time::now();
    let cfg = AgentSection::default();
    // No handoff waiting: no wake.
    assert!(!should_wake(&app.db, &session, &cfg, &now));
    seed_handoff(&app, "sa_1", &session, "预算是 42");
    assert!(should_wake(&app.db, &session, &cfg, &now));
    // Feature off: no wake.
    let off = AgentSection {
        wake_on_handoff: false,
        ..AgentSection::default()
    };
    assert!(!should_wake(&app.db, &session, &off, &now));
    // Hourly cap reached: no wake.
    for _ in 0..cfg.wake_max_per_hour {
        app.db.audit("wake:test", WAKE_EVENT, "x").unwrap();
    }
    assert!(!should_wake(&app.db, &session, &cfg, &now));
}

#[test]
fn sessions_with_pending_lists_only_sessions() {
    let (_dir, app, session) = setup();
    seed_handoff(&app, "sa_1", &session, "预算是 42");
    // A sub-agent parent's handoff never wakes a session.
    let now = hatch_core::time::now();
    let store = RunStore::new(&app.db);
    store
        .create(
            "sa_nested",
            "subagent:sa_1",
            &session,
            2,
            "explore",
            "子任务",
            RunStatus::Completed,
            &now,
        )
        .unwrap();
    store.save_handoff("sa_nested", "嵌套结果", false, &now).unwrap();
    let sessions = store.sessions_with_pending_handoffs().unwrap();
    assert_eq!(sessions, vec![session.clone()]);
    assert!(store
        .has_pending_handoffs(&format!("session:{session}"))
        .unwrap());
}

#[test]
fn wake_turn_reports_and_marks_delivered() {
    let (_dir, app, session) = setup();
    seed_handoff(&app, "sa_1", &session, "预算是 42");
    let router = scripted_router("子代理查完了：预算是 42。");
    let outcome = run_wake_turn(&app, &session, &router, CancelToken::new(), None).unwrap();
    assert!(!outcome.silent);
    assert!(outcome.answer.unwrap().contains("预算是 42"));
    // Exactly-once: the handoff is consumed by the wake turn.
    let parent = format!("session:{session}");
    assert!(!RunStore::new(&app.db).has_pending_handoffs(&parent).unwrap());
    // The wake turn is on the separate audit trail.
    let tail = app.db.audit_tail(10).unwrap();
    assert!(tail.iter().any(|a| a.event == WAKE_EVENT));
    // The report is in the session history.
    let (_, history) = SessionStore::new(&app.db).active(&session).unwrap();
    assert!(history
        .iter()
        .any(|m| m.message.text_content().contains("预算是 42")));
}

#[test]
fn wake_turn_silent_sentinel() {
    let (_dir, app, session) = setup();
    seed_handoff(&app, "sa_1", &session, "例行检查完成，无异常");
    let router = scripted_router("无事可报");
    let outcome = run_wake_turn(&app, &session, &router, CancelToken::new(), None).unwrap();
    assert!(outcome.silent);
    assert!(outcome.answer.is_none());
    let parent = format!("session:{session}");
    assert!(!RunStore::new(&app.db).has_pending_handoffs(&parent).unwrap());
}
