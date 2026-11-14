//! End-to-end tests against a real Chromium and the local mock site.
//! Skipped (with a message) when no Chromium is installed.

use hatch_browser::testkit::MockSite;
use hatch_browser::{find_chromium, Broker, BrowserConfig, BrowserError, ClickExpect, PageCondition, SessionState, SessionStore};
use hatch_store::Db;

fn setup() -> Option<(Broker, Db, MockSite, tempfile::TempDir)> {
    if find_chromium(None).is_none() {
        eprintln!("SKIP: no Chromium found (set HATCH_CHROMIUM to run the browser tests)");
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = BrowserConfig::new(dir.path());
    cfg.allow_loopback = true; // the mock site lives on 127.0.0.1 — tests only
    cfg.ca_bundle = None;
    cfg.max_tabs = 8;
    Some((Broker::new(cfg), Db::open_in_memory().unwrap(), MockSite::start().unwrap(), dir))
}

fn reference(view: &hatch_browser::PageView, text: &str) -> u32 {
    view.snapshot.elements.iter().find(|e| e.display_name().contains(text)).unwrap_or_else(|| panic!("no element {text}: {}", view.render())).reference
}

#[test]
fn read_navigate_select_and_find() {
    let Some((b, db, site, _d)) = setup() else { return };
    let v = b.open(&db, "chat:s1", &site.url("/article"), None).unwrap();
    assert_eq!(v.condition, PageCondition::Ok);
    assert!(v.render().contains("青鸟"), "{}", v.render());
    assert_eq!(SessionStore::new(&db).get(&v.session).unwrap().state, SessionState::Ready);
    assert!(b.find(&db, &v.session, "标签页", None).unwrap()[0].contains("4 个标签页"));

    let next = b.click(&db, &v.session, reference(&v, "下一篇"), None, None).unwrap();
    assert!(next.navigated && next.snapshot.url.ends_with("/article/next"), "{}", next.render());
    let back = b.back(&db, &v.session, None).unwrap();
    assert!(back.snapshot.url.ends_with("/article"));

    let s = b.open(&db, "chat:s1", &site.url("/search"), None).unwrap();
    let city = reference(&s, "城市");
    assert_eq!(b.select(&db, &s.session, city, "上海", None).unwrap(), "上海");
    match b.select(&db, &s.session, city, "拉萨", None) {
        Err(BrowserError::NoSuchOption { available, .. }) => assert_eq!(available, vec!["北京", "上海", "广州", "深圳"]),
        other => panic!("expected NoSuchOption, got {other:?}"),
    }
    let shot = b.screenshot(&db, &s.session, None).unwrap();
    assert_eq!(&std::fs::read(&shot).unwrap()[..4], b"\x89PNG");
}

#[test]
fn approved_submission_runs_once_and_only_as_approved() {
    let Some((b, db, site, _d)) = setup() else { return };
    let v = b.open(&db, "chat:s1", &site.url("/contact"), None).unwrap();
    b.type_text(&db, &v.session, reference(&v, "姓名"), "张三", None).unwrap();
    b.type_text(&db, &v.session, reference(&v, "邮箱"), "zhang@example.com", None).unwrap();
    b.type_text(&db, &v.session, reference(&v, "留言"), "想了解企业版价格", None).unwrap();
    let submit = reference(&v, "提交");
    let (el, page) = b.describe(&db, &v.session, submit, None).unwrap();
    let form = b.form_fields(&db, &v.session, submit, None).unwrap();
    assert_eq!(form.iter().map(|f| f.value.as_str()).collect::<Vec<_>>(), vec!["张三", "zhang@example.com", "想了解企业版价格"]);
    let expect = ClickExpect { page_url: page.url.clone(), element: el.clone(), form: form.clone() };

    // A tampered expectation (different form content) is refused, nothing is sent.
    let mut wrong = expect.clone();
    wrong.form[2].value = "别的内容".into();
    assert!(matches!(b.click(&db, &v.session, submit, Some(&wrong), None), Err(BrowserError::PolicyDenied(_))));
    assert!(site.submissions_to("/contact").is_empty());

    let done = b.click(&db, &v.session, submit, Some(&expect), None).unwrap();
    assert!(done.render().contains("C-1001"), "{}", done.render());
    assert_eq!(site.submissions_to("/contact").len(), 1);
    assert!(site.submissions_to("/contact")[0].body.contains("想了解企业版价格"));
    // Replaying the same approval on the changed page does nothing.
    assert!(b.click(&db, &v.session, submit, Some(&expect), None).is_err());
    assert_eq!(site.submissions_to("/contact").len(), 1);
}

#[test]
fn login_captcha_and_blocked_pages_are_reported_honestly() {
    let Some((b, db, site, _d)) = setup() else { return };
    let login = b.open(&db, "chat:s1", &site.url("/account"), None).unwrap();
    assert!(matches!(login.condition, PageCondition::LoginRequired(_)), "{}", login.render());
    assert_eq!(SessionStore::new(&db).get(&login.session).unwrap().state, SessionState::NeedsUser);
    let pw = login.snapshot.elements.iter().find(|e| e.is_password()).unwrap().reference;
    assert!(b.type_text(&db, &login.session, pw, "hunter2", None).is_err(), "never types passwords");
    assert!(matches!(b.click(&db, &login.session, pw, None, None), Err(BrowserError::SessionUnavailable(_))));
    assert!(b.read(&db, &login.session, None, None).is_ok(), "reading is still allowed");
    // Resume re-validates: the wall is still there, so the probe puts the
    // session straight back to NeedsUser instead of trusting the resume.
    let probe = b.resume(&db, &login.session).unwrap();
    assert!(probe.needs_user(), "{}", probe.render());
    assert_eq!(SessionStore::new(&db).get(&login.session).unwrap().state, SessionState::NeedsUser);

    let cap = b.open(&db, "chat:s1", &site.url("/verify"), None).unwrap();
    assert!(matches!(cap.condition, PageCondition::Captcha(_)), "{}", cap.render());

    let blocked = b.open(&db, "chat:s1", &site.url("/forbidden"), None).unwrap();
    assert!(matches!(blocked.condition, PageCondition::Blocked(_)));
    assert!(blocked.render().contains("不代表页面上没有"));
}

#[test]
fn private_destinations_and_redirects_are_blocked() {
    let Some((b, db, site, _d)) = setup() else { return };
    assert!(matches!(b.open(&db, "chat:s1", "http://10.0.0.1/admin", None), Err(BrowserError::PolicyDenied(_))));
    assert!(matches!(b.open(&db, "chat:s1", "file:///etc/passwd", None), Err(BrowserError::PolicyDenied(_))));
    match b.open(&db, "chat:s1", &site.url("/go-internal"), None) {
        Err(BrowserError::PolicyDenied(m)) => assert!(m.contains("10.0.0.1"), "{m}"),
        other => panic!("redirect to a private address must be blocked, got {other:?}"),
    }
    assert!(SessionStore::new(&db).list(false).unwrap().is_empty(), "failed opens leave no sessions");
}

#[test]
fn profiles_are_isolated_and_other_processes_can_attach() {
    let Some((b, db, site, d)) = setup() else { return };
    let main = b.open(&db, "chat:s1", &site.url("/set-cookie"), None).unwrap();
    let mine = b.navigate(&db, &main.session, &site.url("/whoami"), None).unwrap();
    assert!(mine.render().contains("hatch_demo=signed-in"), "{}", mine.render());
    let research = b.open(&db, "subagent:sa_1", &site.url("/whoami"), None).unwrap();
    assert_eq!(research.profile, "research");
    assert!(research.render().contains("cookie: []"), "research profile must not see main's cookies: {}", research.render());

    // A second broker (as in another hatch process) attaches to the running browser.
    let mut cfg = BrowserConfig::new(d.path());
    cfg.allow_loopback = true;
    let other = Broker::new(cfg);
    let again = other.read(&db, &main.session, None, None).unwrap();
    assert!(again.snapshot.url.ends_with("/whoami"));
    let pid = b.status(&db).into_iter().find(|p| p.profile == "main").unwrap().pid.unwrap();
    drop(other);
    assert!(hatch_browser::launch::pid_alive(pid), "an attached broker does not stop someone else's browser");
    drop(b);
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(!hatch_browser::launch::pid_alive(pid), "the owner stops its browser on exit");
}

#[test]
fn download_lands_in_profile_download_dir() {
    let Some((b, db, site, _d)) = setup() else { return };
    let v = b.open(&db, "chat:s1", &site.url("/report"), None).unwrap();
    assert!(v.render().contains("下载报表"), "{}", v.render());
    let dir = b.enable_downloads(&db, &v.session).unwrap();
    let link = reference(&v, "下载报表");
    let _after = b.click(&db, &v.session, link, None, None).unwrap();
    // Local file: fast, but poll for the completed event.
    let mut done = None;
    for _ in 0..100 {
        let list = b.downloads(&db, &v.session).unwrap();
        if let Some(d) = list.iter().find(|d| d.filename == "report.csv" && d.state == hatch_browser::DownloadState::Completed) {
            done = Some(d.clone());
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let d = done.expect("report.csv download did not complete");
    let path = d.path.clone().expect("a completed download reports its path");
    assert!(path.starts_with(&dir), "{path:?} not under {dir:?}");
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("上海,1200"), "{body}");
    assert_eq!(body.lines().count(), 4, "{body}");
    // A second listing still reports the file (events were already drained).
    let again = b.downloads(&db, &v.session).unwrap();
    assert!(again.iter().any(|d| d.filename == "report.csv" && d.state == hatch_browser::DownloadState::Completed));
}


#[test]
fn needs_user_writes_task_event_and_resume_probes_the_wall() {
    let Some((b, db, site, _d)) = setup() else { return };
    // Create the task row first: the broker's event is control-plane and
    // requires the task to exist (a non-task origin simply skips it).
    db.conn()
        .execute(
            "INSERT INTO tasks(id, kind, title, owner, status, last_heartbeat, created_at, updated_at) VALUES('tsk_probe1', 'meeting-brief', 'probe', 'test', 'running', '2026-11-14T00:00:00Z', '2026-11-14T00:00:00Z', '2026-11-14T00:00:00Z')",
            [],
        )
        .unwrap();
    let v = b.open(&db, "task:tsk_probe1", &site.url("/orders"), None).unwrap();
    assert!(v.needs_user(), "{}", v.render());
    assert_eq!(SessionStore::new(&db).get(&v.session).unwrap().state, SessionState::NeedsUser);
    let events = hatch_tasks::events::EventLog::new(&db).events_since("tsk_probe1", 0, 10).unwrap();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].kind, "browser_needs_user");
    assert!(events[0].detail.contains("需要登录"), "{}", events[0].detail);
    assert!(events[0].detail.contains("/orders"), "{}", events[0].detail);

    // Resume without logging in: the probe re-opens the wall, the session
    // goes straight back to NeedsUser.
    let probe = b.resume(&db, &v.session).unwrap();
    assert!(probe.needs_user(), "{}", probe.render());
    assert_eq!(SessionStore::new(&db).get(&v.session).unwrap().state, SessionState::NeedsUser);

    // Log in through a second session in the same profile (cookies are
    // per-profile), then resume: the probe sees the orders page.
    let login = b.open(&db, "chat:s1", &site.url("/set-cookie"), None).unwrap();
    assert!(login.render().contains("已设置登录"), "{}", login.render());
    let probe = b.resume(&db, &v.session).unwrap();
    assert!(!probe.needs_user(), "{}", probe.render());
    assert!(probe.render().contains("A-1001"), "{}", probe.render());
    assert_eq!(SessionStore::new(&db).get(&v.session).unwrap().state, SessionState::Ready);
}
