//! End-to-end CalDAV sync tests against the local mock server only.
//!
//! No real network, no real credentials: the only token is the fake
//! `test-token-caldav`, stored in the vault (via the real vault helper)
//! and sent as a Bearer header by the helper — never by this test.

#[path = "support/caldav_mock.rs"]
mod mock;

use hatch_connectors::caldav::{CaldavClient, Keep, SyncEngine, SyncSettings, VaultHttp};
use hatch_connectors::calendar::Event;
use hatch_connectors::CalendarRepo;
use hatch_core::time::parse_rfc3339;
use hatch_store::Db;
use hatch_vault::client::{StoreRequest, VaultClient};
use mock::{ics, FailMode, MockServer, FAKE_TOKEN};

fn setup() -> (tempfile::TempDir, VaultClient, MockServer, Db) {
    let home = tempfile::tempdir().unwrap();
    let vc = VaultClient::start(home.path()).unwrap();
    vc.store(
        "caldav-test",
        StoreRequest { value: FAKE_TOKEN.into(), ..Default::default() },
    )
    .unwrap();
    let server = MockServer::start();
    let db = Db::open_in_memory().unwrap();
    (home, vc, server, db)
}

fn engine<'a>(vc: &VaultClient, server: &MockServer, db: &'a Db) -> SyncEngine<'a> {
    let tz: chrono_tz::Tz = "Asia/Shanghai".parse().unwrap();
    let http = VaultHttp::new(vc.clone(), "caldav-test".into());
    let client = CaldavClient::new(&server.base_url, Box::new(http)).unwrap();
    let settings = SyncSettings::new(&server.base_url, "tester", "caldav-test");
    SyncEngine::new(db, tz, client, settings)
}

fn seed(server: &MockServer) {
    let mut st = server.state.lock().unwrap();
    st.put_server("u1", ics("u1", "周会", "20261104T020000Z", "20261104T030000Z"));
    st.put_server("u2", ics("u2", "一对一", "20261105T020000Z", "20261105T023000Z"));
}

fn local_title(db: &Db, uid: &str) -> Option<String> {
    CalendarRepo::new(db)
        .all()
        .unwrap()
        .into_iter()
        .find(|e| e.uid == uid)
        .map(|e| e.title)
}

fn ev(uid: &str, title: &str) -> Event {
    Event {
        uid: uid.into(),
        title: title.into(),
        starts_at: parse_rfc3339("2026-11-06T02:00:00Z").unwrap(),
        ends_at: parse_rfc3339("2026-11-06T03:00:00Z").unwrap(),
        location: String::new(),
        source: "test".into(),
    }
}

#[test]
fn full_pull_then_idempotent_resync() {
    let (_home, vc, server, db) = setup();
    seed(&server);
    let eng = engine(&vc, &server, &db);
    let r = eng.sync(false).unwrap();
    assert_eq!(r.pulled_new, 2, "{r:?}");
    assert_eq!(local_title(&db, "u1").as_deref(), Some("周会"));
    assert_eq!(local_title(&db, "u2").as_deref(), Some("一对一"));
    assert!(eng.status().unwrap().has_token(), "sync-token must be stored");

    // Re-run with no changes: nothing pulled/pushed, zero writes.
    let before = server.write_count();
    let r = eng.sync(false).unwrap();
    assert_eq!((r.pulled_new, r.pulled_updated, r.pulled_deleted), (0, 0, 0), "{r:?}");
    assert_eq!((r.pushed_created, r.pushed_updated, r.pushed_deleted), (0, 0, 0), "{r:?}");
    assert_eq!(server.write_count(), before, "idempotent re-sync must not write");
}

#[test]
fn incremental_pull_and_push() {
    let (_home, vc, server, db) = setup();
    seed(&server);
    let eng = engine(&vc, &server, &db);
    eng.sync(false).unwrap();

    // Another client changes the server: u1 edited, u3 added, u2 deleted.
    {
        let mut st = server.state.lock().unwrap();
        st.put_server("u1", ics("u1", "周会（改期）", "20261104T020000Z", "20261104T030000Z"));
        st.put_server("u3", ics("u3", "新会", "20261107T020000Z", "20261107T030000Z"));
        st.delete_server("u2");
    }
    let r = eng.sync(false).unwrap();
    assert_eq!((r.pulled_updated, r.pulled_new, r.pulled_deleted), (1, 1, 1), "{r:?}");
    assert_eq!(local_title(&db, "u1").as_deref(), Some("周会（改期）"));
    assert!(local_title(&db, "u2").is_none());
    assert!(local_title(&db, "u3").is_some());

    // Local changes: create L1, edit u1, delete u3.
    let cal = CalendarRepo::new(&db);
    cal.upsert(&ev("L1", "本地新建")).unwrap();
    let mut u1 = cal.all().unwrap().into_iter().find(|e| e.uid == "u1").unwrap();
    u1.title = "周会（本地改）".into();
    cal.upsert(&u1).unwrap();
    cal.delete("u3").unwrap();
    let before = server.write_count();
    let r = eng.sync(false).unwrap();
    assert_eq!((r.pushed_created, r.pushed_updated, r.pushed_deleted), (1, 1, 1), "{r:?}");
    assert_eq!(server.write_count() - before, 3);
    assert!(server.writes().contains(&"PUT L1".to_string()));
    assert!(server.writes().contains(&"PUT u1".to_string()));
    assert!(server.writes().contains(&"DELETE u3".to_string()));
    // And the push is idempotent too.
    let before = server.write_count();
    let r = eng.sync(false).unwrap();
    assert_eq!((r.pushed_created, r.pushed_updated, r.pushed_deleted), (0, 0, 0), "{r:?}");
    assert_eq!(server.write_count(), before);
    // The server now holds the local u1 title.
    let st = server.state.lock().unwrap();
    assert!(st.events["u1"].ics.contains("本地改"));
}

#[test]
fn conflict_server_wins_then_resolve_local_pushes() {
    let (_home, vc, server, db) = setup();
    seed(&server);
    let eng = engine(&vc, &server, &db);
    eng.sync(false).unwrap();

    // Both sides edit u1.
    let cal = CalendarRepo::new(&db);
    let mut u1 = cal.all().unwrap().into_iter().find(|e| e.uid == "u1").unwrap();
    u1.title = "本地版".into();
    cal.upsert(&u1).unwrap();
    server
        .state
        .lock()
        .unwrap()
        .put_server("u1", ics("u1", "服务器版", "20261104T020000Z", "20261104T030000Z"));
    let before = server.write_count();
    let r = eng.sync(false).unwrap();
    assert_eq!(r.conflicts_new.len(), 1, "{r:?}");
    assert_eq!(r.conflicts_new[0].uid, "u1");
    assert_eq!(r.conflicts_new[0].kind, "edit");
    assert_eq!(r.conflicts_new[0].kept, "server");
    // Server wins locally, and nothing was pushed.
    assert_eq!(local_title(&db, "u1").as_deref(), Some("服务器版"));
    assert_eq!(server.write_count(), before, "a conflict must never push the local version");
    assert_eq!(eng.status().unwrap().open_conflicts, 1);

    // Explicit approval: resolve --keep local, next sync pushes it.
    let msg = eng.resolve("u1", Keep::Local).unwrap();
    assert!(msg.contains("下次同步"), "{msg}");
    assert_eq!(local_title(&db, "u1").as_deref(), Some("本地版"));
    let before = server.write_count();
    let r = eng.sync(false).unwrap();
    assert_eq!(r.pushed_updated, 1, "{r:?}");
    assert_eq!(server.write_count() - before, 1);
    assert!(server.writes().contains(&"PUT u1".to_string()));
    let st = server.state.lock().unwrap();
    assert!(st.events["u1"].ics.contains("本地版"));
    assert_eq!(eng.status().unwrap().open_conflicts, 0);
}

#[test]
fn deletion_conflicts_follow_the_same_rule() {
    let (_home, vc, server, db) = setup();
    seed(&server);
    let eng = engine(&vc, &server, &db);
    eng.sync(false).unwrap();

    // Server deletes u1 while the local side edited it.
    let cal = CalendarRepo::new(&db);
    let mut u1 = cal.all().unwrap().into_iter().find(|e| e.uid == "u1").unwrap();
    u1.title = "本地改过".into();
    cal.upsert(&u1).unwrap();
    server.state.lock().unwrap().delete_server("u1");
    let r = eng.sync(false).unwrap();
    assert_eq!(r.conflicts_new.len(), 1, "{r:?}");
    assert_eq!(r.conflicts_new[0].kind, "server_deleted");
    // Local copy is kept (not deleted) until resolved.
    assert_eq!(local_title(&db, "u1").as_deref(), Some("本地改过"));
    eng.resolve("u1", Keep::Server).unwrap();
    assert!(local_title(&db, "u1").is_none(), "keep server deletes the local copy");

    // Locally deleted vs server edited: server version is restored,
    // the deletion is recorded as a conflict.
    let cal = CalendarRepo::new(&db);
    cal.delete("u2").unwrap();
    server
        .state
        .lock()
        .unwrap()
        .put_server("u2", ics("u2", "服务器又改了", "20261105T020000Z", "20261105T023000Z"));
    let r = eng.sync(false).unwrap();
    assert_eq!(r.conflicts_new.len(), 1, "{r:?}");
    assert_eq!(r.conflicts_new[0].kind, "local_deleted");
    assert_eq!(local_title(&db, "u2").as_deref(), Some("服务器又改了"));
    // Keep local = the deletion stands: resolve re-applies it locally,
    // and the next sync DELETEs it on the server.
    eng.resolve("u2", Keep::Local).unwrap();
    assert!(local_title(&db, "u2").is_none(), "resolve re-applies the local deletion");
    let before = server.write_count();
    let r = eng.sync(false).unwrap();
    assert_eq!(r.pushed_deleted, 1, "{r:?}");
    assert_eq!(server.write_count() - before, 1);
    assert!(server.writes().contains(&"DELETE u2".to_string()));
}

#[test]
fn server_deleted_keep_local_recreates_on_next_sync() {
    let (_home, vc, server, db) = setup();
    seed(&server);
    let eng = engine(&vc, &server, &db);
    eng.sync(false).unwrap();

    // Server deletes u1 while the local side edited it; keep local.
    let cal = CalendarRepo::new(&db);
    let mut u1 = cal.all().unwrap().into_iter().find(|e| e.uid == "u1").unwrap();
    u1.title = "本地改过".into();
    cal.upsert(&u1).unwrap();
    server.state.lock().unwrap().delete_server("u1");
    let r = eng.sync(false).unwrap();
    assert_eq!(r.conflicts_new.len(), 1, "{r:?}");
    assert_eq!(r.conflicts_new[0].kind, "server_deleted");
    eng.resolve("u1", Keep::Local).unwrap();
    assert_eq!(local_title(&db, "u1").as_deref(), Some("本地改过"));

    // Next sync re-creates it on the server (PUT, If-None-Match *).
    let before = server.write_count();
    let r = eng.sync(false).unwrap();
    assert_eq!(r.pushed_updated, 1, "{r:?}");
    assert_eq!(server.write_count() - before, 1);
    assert!(server.writes().contains(&"PUT u1".to_string()));
    let st = server.state.lock().unwrap();
    assert!(st.events["u1"].ics.contains("本地改过"));
}

#[test]
fn dry_run_changes_nothing() {
    let (_home, vc, server, db) = setup();
    seed(&server);
    let eng = engine(&vc, &server, &db);
    eng.sync(false).unwrap();
    server
        .state
        .lock()
        .unwrap()
        .put_server("u1", ics("u1", "服务器新标题", "20261104T020000Z", "20261104T030000Z"));
    let cal = CalendarRepo::new(&db);
    cal.upsert(&ev("L9", "本地新建")).unwrap();

    let r = eng.sync(true).unwrap();
    assert!(r.dry_run);
    assert_eq!(r.pulled_updated, 1, "{r:?}");
    assert_eq!(r.pushed_created, 1, "{r:?}");
    assert_eq!(server.write_count(), 0, "dry-run must not write to the server");
    assert_eq!(local_title(&db, "u1").as_deref(), Some("周会"), "dry-run must not apply the pull");
    assert!(eng.status().unwrap().pending_new >= 1, "L9 is still pending");

    // The real sync then applies the same plan.
    let r = eng.sync(false).unwrap();
    assert_eq!((r.pulled_updated, r.pushed_created), (1, 1), "{r:?}");
}

#[test]
fn auth_server_and_malformed_errors_are_honest() {
    let (_home, vc, server, db) = setup();
    seed(&server);
    let eng = engine(&vc, &server, &db);

    // 401 fails fast at discovery (no REPORT is ever attempted, and
    // 4xx is never retried).
    server.state.lock().unwrap().fail = FailMode::Unauthorized;
    let err = eng.sync(false).unwrap_err();
    assert!(err.to_string().contains("401"), "{err}");
    let calls = server.state.lock().unwrap().calls.clone();
    assert!(calls.iter().all(|c| c.starts_with("PROPFIND")), "401 must fail at discovery: {calls:?}");
    assert!(!calls.iter().any(|c| c.starts_with("REPORT")), "{calls:?}");

    // 500 is retried exactly once per call, then reported — never a storm.
    server.state.lock().unwrap().fail = FailMode::ServerError;
    let propfinds_before = server
        .state
        .lock()
        .unwrap()
        .calls
        .iter()
        .filter(|c| c.starts_with("PROPFIND"))
        .count();
    let err = eng.sync(false).unwrap_err();
    assert!(err.to_string().contains("500"), "{err}");
    let propfinds = server
        .state
        .lock()
        .unwrap()
        .calls
        .iter()
        .filter(|c| c.starts_with("PROPFIND"))
        .count();
    assert_eq!(propfinds - propfinds_before, 2, "one retry on transient 5xx, then stop");

    // Malformed XML is an error, not empty data. Discovery works
    // (PROPFIND is fine); the REPORT fails, the fetch_range fallback
    // fails the same way, and the error surfaces.
    server.state.lock().unwrap().fail = FailMode::MalformedXml;
    let err = eng.sync(false).unwrap_err();
    assert!(!err.to_string().is_empty(), "malformed XML must be an error, not empty data");
    let reports = server
        .state
        .lock()
        .unwrap()
        .calls
        .iter()
        .filter(|c| c.starts_with("REPORT"))
        .count();
    assert_eq!(reports, 2, "sync-collection + one fallback fetch, no retry on parse errors");

    // Recovery: clear the failure, sync works again.
    server.state.lock().unwrap().fail = FailMode::None;
    let r = eng.sync(false).unwrap();
    assert_eq!(r.pulled_new, 2, "{r:?}");
}
