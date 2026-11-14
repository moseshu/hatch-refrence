//! Task 8 retention sets: the case sets the system must keep passing.
//!
//! * Unknown-website set — the browser-basic world (9 tasks over an
//!   unknown site: read, choose, log in, captcha, contact, pay, an
//!   injection attempt, a redirect to a private address, a download).
//!   This test pins the inventory, so dropping a task fails the set
//!   instead of silently shrinking it.
//! * Overnight set — a scheduled weekly-report job across a simulated
//!   night with a process restart in the middle (scaled clock: explicit
//!   timestamps, no waiting). The report is delivered exactly once to
//!   the private inbox: no duplicate after the restart, no second
//!   report inside the same ISO week, a fresh one the next week.

use chrono::Duration;
use hatch_core::time::{parse_rfc3339, to_rfc3339, Timestamp};
use hatch_core::CancelToken;
use hatch_policy::{AttentionPolicy, Notifier, Priority};
use hatch_scheduler::{Engine, JobStore, NewJob};
use hatch_store::{params, Db};
use hatch_triage::Category;
use hatch_workers::seed::{self, SeedMail};
use hatch_workers::weekly_report::{self, WeeklyReportWorker};
use hatch_workers::WorkerOutputs;
use std::path::PathBuf;
use std::sync::Arc;

fn ts(s: &str) -> Timestamp {
    parse_rfc3339(s).unwrap()
}

#[test]
fn unknown_website_set_is_complete() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../eval/worlds/browser-basic");
    let world: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("browser.json")).unwrap()).unwrap();
    let tasks: Vec<&str> = world["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["id"].as_str())
        .collect();
    assert_eq!(
        tasks,
        ["article", "options", "login", "captcha", "contact", "checkout", "injection", "redirect", "download"],
        "the unknown-website set must keep all nine tasks"
    );
    let never: Vec<&str> = world["expectations"]["never_posted"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(never.contains(&"/delete-account"), "injection guard must stay: {never:?}");
    assert!(never.contains(&"/pay"), "payment guard must stay: {never:?}");
    assert!(
        world["expectations"]["answer_all"]["download"].is_array(),
        "the download task must keep its answer expectation"
    );
}

fn seed_week(db: &Db, now: &Timestamp) {
    seed::mail(
        db,
        "moses@126.example",
        &SeedMail {
            message_id: "<r1@corp>",
            from_addr: "wang@corp.example",
            from_name: "王经理",
            subject: "Q4 预算请今天确认",
            category: Category::Urgent,
            priority: Priority::parse("urgent").unwrap(),
            summary: "财务 18:00 前要最终数字",
            needs_reply: true,
            suspicious: false,
        },
        now,
    )
    .unwrap();
    seed::event(db, "ev-week", "架构评审", ts("2026-11-03T20:00:00Z"), 60, "").unwrap();
    db.conn()
        .execute(
            "INSERT INTO tasks(id, kind, title, owner, status, last_heartbeat, created_at, updated_at, finished_at)
             VALUES('tsk-week', 'meeting-brief', '本周已完成：架构评审纪要', 'test', 'done', ?1, ?1, ?1, ?2)",
            params![to_rfc3339(now), to_rfc3339(&(*now - Duration::days(1)))],
        )
        .unwrap();
}

fn worker(tz: chrono_tz::Tz) -> WeeklyReportWorker {
    WeeklyReportWorker {
        router: None,
        attention: AttentionPolicy { tz, ..Default::default() },
        user_name: "Moses".into(),
        mail_configured: true,
    }
}

fn reports(db: &Db) -> usize {
    WorkerOutputs::new(db)
        .recent(10)
        .unwrap_or_default()
        .into_iter()
        .filter(|o| o.kind == "weekly-report")
        .count()
}

fn report_notifications(db: &Db, tz: chrono_tz::Tz) -> usize {
    Notifier::new(db, AttentionPolicy { tz, ..Default::default() })
        .pending(false)
        .unwrap_or_default()
        .into_iter()
        .filter(|n| n.text.contains("周报已生成"))
        .count()
}

#[test]
fn overnight_weekly_report_survives_restart_and_delivers_once() {
    let home = tempfile::tempdir().unwrap();
    let db_path = home.path().join("hatch.db");
    let tz: chrono_tz::Tz = "America/New_York".parse().unwrap();
    // Monday 2026-11-02 08:00 EST — the weekly job's due time.
    let monday = ts("2026-11-02T13:00:00Z");
    let cancel = CancelToken::new();

    // Process 1: seed the week, run the job once.
    {
        let db = Db::open(&db_path).unwrap();
        seed_week(&db, &monday);
        let e = Engine::new(&db, tz).register(weekly_report::KIND, Arc::new(worker(tz)));
        JobStore::new(&db)
            .add(
                &NewJob::new("weekly-report", weekly_report::KIND, "daily 08:00"),
                &(monday - Duration::minutes(1)),
                &tz,
            )
            .unwrap();
        e.tick(&monday, &cancel).unwrap();
        assert_eq!(reports(&db), 1, "one report after the first run");
        assert_eq!(report_notifications(&db, tz), 1, "one notification after the first run");
    } // The process exits here: engine and DB handle both drop.

    // Process 2: a fresh engine over the same database file.
    let db = Db::open(&db_path).unwrap();
    let e = Engine::new(&db, tz).register(weekly_report::KIND, Arc::new(worker(tz)));
    // Same morning, after the restart: a re-run must not duplicate.
    e.tick(&(monday + Duration::hours(2)), &cancel).unwrap();
    e.run_now("weekly-report", &(monday + Duration::hours(3)), &cancel).unwrap();
    assert_eq!(reports(&db), 1, "same week: no second report after restart");
    assert_eq!(report_notifications(&db, tz), 1, "same week: no second notification after restart");
    // Tuesday: still the same ISO week.
    e.tick(&(monday + Duration::days(1)), &cancel).unwrap();
    assert_eq!(reports(&db), 1, "Tuesday is the same ISO week");
    assert_eq!(report_notifications(&db, tz), 1);
    // Next Monday: a new ISO week gets exactly one fresh report.
    e.tick(&(monday + Duration::days(7)), &cancel).unwrap();
    assert_eq!(reports(&db), 2, "next week gets a new report");
    assert_eq!(report_notifications(&db, tz), 2, "next week gets a new notification");
}
