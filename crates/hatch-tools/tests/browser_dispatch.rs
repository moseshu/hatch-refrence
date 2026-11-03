//! Browser tools through the real dispatcher (gate → approval → executor)
//! against a real Chromium and the mock site. Skipped without Chromium.

use hatch_browser::testkit::MockSite;
use hatch_browser::{find_chromium, Broker, BrowserConfig};
use hatch_core::{CancelToken, Trust};
use hatch_llm::ToolCall;
use hatch_policy::{ApprovalQueue, ApprovalStatus, Gatekeeper, PolicyConfig};
use hatch_store::Db;
use hatch_tools::builtin::browser::add_browser_tools;
use hatch_tools::{dispatch_call, execute_approved, BrowserClickExecutor, BrowserOpenExecutor, ExecutorRegistry, Registry, ToolContext};
use serde_json::json;
use std::sync::Arc;

struct Env {
    db: Db,
    reg: Registry,
    exec: ExecutorRegistry,
    gate: Gatekeeper,
    site: MockSite,
    _dir: tempfile::TempDir,
}

fn env() -> Option<Env> {
    if find_chromium(None).is_none() {
        eprintln!("SKIP: no Chromium found");
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = BrowserConfig::new(dir.path());
    cfg.allow_loopback = true;
    cfg.ca_bundle = None;
    let broker = Arc::new(Broker::new(cfg));
    Some(Env {
        db: Db::open_in_memory().unwrap(),
        reg: add_browser_tools(Registry::new(), broker.clone()),
        exec: ExecutorRegistry::new().with(Arc::new(BrowserOpenExecutor { broker: broker.clone() })).with(Arc::new(BrowserClickExecutor { broker })),
        gate: Gatekeeper::new(PolicyConfig::default()),
        site: MockSite::start().unwrap(),
        _dir: dir,
    })
}

fn ctx<'a>(db: &'a Db, origin: &str, tainted: bool, call: &str) -> ToolContext<'a> {
    ToolContext {
        db,
        now: hatch_core::time::now(),
        tz: chrono_tz::Asia::Shanghai,
        user_email: "me@example.com".into(),
        origin: origin.into(),
        tainted,
        call_id: call.into(),
        cancel: CancelToken::new(),
    }
}

fn call(e: &Env, origin: &str, tainted: bool, name: &str, args: serde_json::Value) -> hatch_tools::DispatchResult {
    let id = format!("c{}", hatch_core::ids::random_id("x"));
    dispatch_call(&e.reg, &e.exec, &e.gate, &ToolCall { id: id.clone(), name: name.into(), arguments: args }, &ctx(&e.db, origin, tainted, &id))
}

fn session_of(text: &str) -> String {
    text.split("浏览器会话 ").nth(1).and_then(|r| r.split('（').next()).unwrap().trim().to_string()
}

fn element(text: &str, needle: &str) -> u64 {
    text.lines()
        .find(|l| l.starts_with('[') && l.contains(needle))
        .and_then(|l| l[1..].split(']').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no element {needle} in:\n{text}"))
}

#[test]
fn submission_waits_for_approval_and_runs_exactly_once() {
    let Some(e) = env() else { return };
    let o = "chat:s1";
    let open = call(&e, o, false, "browser_open", json!({"url": e.site.url("/contact")}));
    assert!(open.content.starts_with("已执行"), "{}", open.content);
    assert_eq!(open.trust, Trust::Untrusted, "page content is untrusted");
    let s = session_of(&open.content);
    for (field, value) in [("姓名", "李四"), ("邮箱", "li@example.com"), ("留言", "请联系我")] {
        let r = call(&e, o, true, "browser_type", json!({"session": s, "ref": element(&open.content, field), "text": value}));
        assert!(r.content.contains("尚未提交"), "typing is local even when tainted: {}", r.content);
    }
    let submit = call(&e, o, true, "browser_click", json!({"session": s, "ref": element(&open.content, "提交")}));
    assert!(submit.content.starts_with("已提交审批"), "{}", submit.content);
    assert!(e.site.submissions().is_empty(), "nothing is sent before approval");
    let q = ApprovalQueue::new(&e.db);
    let a = q.get(submit.approval_id.as_deref().unwrap()).unwrap();
    assert_eq!(a.class, hatch_policy::ActionClass::BrowserSubmit);
    assert!(a.preview.contains("请联系我") && a.preview.contains("/contact"), "{}", a.preview);

    let now = hatch_core::time::now();
    q.decide(&a.id, true, "ok", &now).unwrap();
    execute_approved(&e.db, &e.exec, &a.id, &now).unwrap();
    assert_eq!(q.get(&a.id).unwrap().status, ApprovalStatus::Executed);
    assert_eq!(e.site.submissions_to("/contact").len(), 1);
    assert!(execute_approved(&e.db, &e.exec, &a.id, &now).is_err(), "an executed approval cannot run again");
    assert_eq!(e.site.submissions_to("/contact").len(), 1, "no second submission");
}

#[test]
fn payments_are_denied_and_injected_clicks_wait_for_the_user() {
    let Some(e) = env() else { return };
    let o = "chat:s2";
    let pay = call(&e, o, false, "browser_open", json!({"url": e.site.url("/checkout")}));
    let s = session_of(&pay.content);
    let r = call(&e, o, true, "browser_click", json!({"session": s, "ref": element(&pay.content, "立即支付")}));
    assert!(r.content.contains("被策略拒绝") && r.content.contains("payment"), "{}", r.content);

    let inj = call(&e, o, false, "browser_open", json!({"url": e.site.url("/inject")}));
    let s2 = session_of(&inj.content);
    let r = call(&e, o, true, "browser_click", json!({"session": s2, "ref": element(&inj.content, "删除账户")}));
    assert!(r.content.starts_with("已提交审批"), "{}", r.content);
    assert!(e.site.submissions().is_empty(), "no payment, no deletion");

    // A tainted turn needs approval to open a new page; another conversation cannot use this session.
    let t = call(&e, o, true, "browser_open", json!({"url": e.site.url("/article")}));
    assert!(t.content.starts_with("已提交审批"), "{}", t.content);
    let other = call(&e, "chat:other", false, "browser_read", json!({"session": s2}));
    assert!(other.content.contains("另一个对话"), "{}", other.content);
}

#[test]
fn login_walls_refuse_passwords_and_private_targets_are_refused() {
    let Some(e) = env() else { return };
    let o = "chat:s3";
    let login = call(&e, o, false, "browser_open", json!({"url": e.site.url("/account")}));
    assert!(login.content.contains("[login_required]") && login.content.contains("hatch browser takeover"), "{}", login.content);
    let s = session_of(&login.content);
    let pw = call(&e, o, true, "browser_type", json!({"session": s, "ref": element(&login.content, "密码框"), "text": "hunter2"}));
    assert!(pw.content.starts_with("ERROR"), "{}", pw.content);
    let blocked = call(&e, o, false, "browser_open", json!({"url": "http://169.254.169.254/latest/meta-data"}));
    assert!(blocked.content.contains("ERROR") && blocked.content.contains("内网"), "{}", blocked.content);
    let redirect = call(&e, o, false, "browser_open", json!({"url": e.site.url("/go-internal")}));
    assert!(redirect.content.contains("10.0.0.1") && redirect.content.contains("policy_denied"), "{}", redirect.content);
}
