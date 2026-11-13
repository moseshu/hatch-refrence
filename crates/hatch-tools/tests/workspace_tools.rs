//! File, shell and web tools end to end: gate → approval → executor.

use hatch_llm::ToolCall;
use hatch_policy::{ActionClass, ApprovalQueue, ApprovalStatus, Gatekeeper, GrantStore, NewGrant, PolicyConfig};
use hatch_sandbox::{HelperCmd, Limits, Sandbox, SandboxMode};
use hatch_store::Db;
use hatch_tools::builtin::{add_general_tools, GeneralTools};
use hatch_tools::{
    dispatch_call, execute_approved, CheckpointStore, ExecCommandExecutor, ExecutorRegistry, FetchUrlExecutor, Registry, ToolContext,
    Workspace, WriteFileExecutor,
};
use serde_json::json;
use std::sync::Arc;

struct Env {
    db: Db,
    dir: tempfile::TempDir,
    ws: Workspace,
    reg: Registry,
    exec: ExecutorRegistry,
    gate: Gatekeeper,
}

fn sandbox() -> Sandbox {
    match HelperCmd::locate() {
        Some(h) => Sandbox { mode: SandboxMode::Namespaces, helper: Some(h), limits: Limits::default(), max_output: 16 * 1024 },
        // Without a helper binary the command tests below still check the
        // policy path; execution is labelled 未隔离.
        None => Sandbox { mode: SandboxMode::None, helper: None, limits: Limits::default(), max_output: 16 * 1024 },
    }
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("config")).unwrap();
    std::fs::write(dir.path().join("config/settings.ini"), "name = demo\ntimeout = 30\n").unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let reg = add_general_tools(
        Registry::new(),
        &GeneralTools { workspace: Some(ws.clone()), web_fetch: true, search_backend: None, max_command_timeout_secs: 60 },
    );
    let exec = ExecutorRegistry::new()
        .with(Arc::new(WriteFileExecutor))
        .with(Arc::new(ExecCommandExecutor { sandbox: Arc::new(sandbox()) }))
        .with(Arc::new(FetchUrlExecutor { limits: Default::default() }));
    Env { db: Db::open_in_memory().unwrap(), dir, ws, reg, exec, gate: Gatekeeper::new(PolicyConfig::default()) }
}

fn ctx<'a>(db: &'a Db, tainted: bool, call: &str) -> ToolContext<'a> {
    ToolContext {
        db,
        now: hatch_core::time::now(),
        tz: chrono_tz::Asia::Shanghai,
        user_email: "me@x.com".into(),
        user_name: String::new(),
        origin: "chat:s".into(),
        tainted,
        call_id: call.into(),
        cancel: hatch_core::CancelToken::new(),
    }
}

fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall { id: "c".into(), name: name.into(), arguments: args }
}

fn approve_and_run(e: &Env, id: &str) -> String {
    let now = hatch_core::time::now();
    ApprovalQueue::new(&e.db).decide(id, true, "test", &now).unwrap();
    execute_approved(&e.db, &e.exec, id, &now).unwrap();
    ApprovalQueue::new(&e.db).get(id).unwrap().note
}

#[test]
fn reads_are_untrusted_and_confined() {
    let e = env();
    let r = dispatch_call(&e.reg, &e.exec, &e.gate, &call("fs_read", json!({"path": "config/settings.ini"})), &ctx(&e.db, false, "1"));
    assert!(r.content.contains("timeout = 30"), "{}", r.content);
    assert_eq!(r.trust, hatch_core::Trust::Untrusted);
    for bad in ["../x", "/etc/passwd", "config/../../x"] {
        let r = dispatch_call(&e.reg, &e.exec, &e.gate, &call("fs_read", json!({"path": bad})), &ctx(&e.db, false, "2"));
        assert!(r.content.starts_with("ERROR"), "{bad}: {}", r.content);
    }
    let r = dispatch_call(&e.reg, &e.exec, &e.gate, &call("fs_grep", json!({"pattern": "timeout"})), &ctx(&e.db, false, "3"));
    assert!(r.content.contains("config/settings.ini:2:"), "{}", r.content);
    let r = dispatch_call(&e.reg, &e.exec, &e.gate, &call("fs_glob", json!({"pattern": "**/*.ini"})), &ctx(&e.db, false, "4"));
    assert!(r.content.contains("config/settings.ini"));
}

#[test]
fn edit_goes_through_approval_with_diff_and_checkpoint() {
    let e = env();
    let args = json!({"path": "config/settings.ini", "old_text": "timeout = 30", "new_text": "timeout = 60"});
    let r = dispatch_call(&e.reg, &e.exec, &e.gate, &call("fs_edit", args), &ctx(&e.db, false, "1"));
    assert!(r.content.starts_with("已提交审批"), "{}", r.content);
    let id = r.approval_id.unwrap();
    let a = ApprovalQueue::new(&e.db).get(&id).unwrap();
    assert!(a.preview.contains("-timeout = 30") && a.preview.contains("+timeout = 60"), "{}", a.preview);
    assert_eq!(std::fs::read_to_string(e.dir.path().join("config/settings.ini")).unwrap(), "name = demo\ntimeout = 30\n", "nothing written before approval");
    let note = approve_and_run(&e, &id);
    assert!(note.contains("检查点"), "{note}");
    assert_eq!(std::fs::read_to_string(e.dir.path().join("config/settings.ini")).unwrap(), "name = demo\ntimeout = 60\n");
    let cps = CheckpointStore::new(&e.db).list(10).unwrap();
    assert_eq!(cps.len(), 1);
    CheckpointStore::new(&e.db).rewind(&cps[0].id, &hatch_core::time::now()).unwrap();
    assert_eq!(std::fs::read_to_string(e.dir.path().join("config/settings.ini")).unwrap(), "name = demo\ntimeout = 30\n");
}

#[test]
fn stale_write_is_refused() {
    let e = env();
    let r = dispatch_call(&e.reg, &e.exec, &e.gate, &call("fs_write", json!({"path": "config/settings.ini", "content": "x\n"})), &ctx(&e.db, false, "1"));
    let id = r.approval_id.unwrap();
    std::fs::write(e.dir.path().join("config/settings.ini"), "changed by the user\n").unwrap();
    approve_and_run(&e, &id);
    assert_eq!(ApprovalQueue::new(&e.db).get(&id).unwrap().status, ApprovalStatus::Failed);
    assert_eq!(std::fs::read_to_string(e.dir.path().join("config/settings.ini")).unwrap(), "changed by the user\n");
}

#[test]
fn grants_waive_writes_but_never_tainted_commands() {
    if hatch_sandbox::HelperCmd::locate().is_some() && !hatch_sandbox::sandbox_works() {
        eprintln!("SKIP: sandbox helper is present but does not run on this host");
        return;
    }
    let e = env();
    let now = hatch_core::time::now();
    let grants = GrantStore::new(&e.db);
    grants
        .add(&NewGrant { class: ActionClass::WriteFile, recipients: vec!["notes/*".into()], max_uses: None, allow_when_tainted: false, expires_at: None, note: String::new() }, &now)
        .unwrap();
    grants
        .add(&NewGrant { class: ActionClass::ExecCommand, recipients: vec!["echo *".into()], max_uses: None, allow_when_tainted: true, expires_at: None, note: String::new() }, &now)
        .unwrap();
    let r = dispatch_call(&e.reg, &e.exec, &e.gate, &call("fs_write", json!({"path": "notes/a.md", "content": "hi\n"})), &ctx(&e.db, false, "1"));
    assert!(r.content.starts_with("已执行"), "{}", r.content);
    assert!(e.dir.path().join("notes/a.md").exists());
    let r = dispatch_call(&e.reg, &e.exec, &e.gate, &call("fs_write", json!({"path": "src/x.rs", "content": "x"})), &ctx(&e.db, false, "2"));
    assert!(r.content.starts_with("已提交审批"), "outside the grant: {}", r.content);
    let r = dispatch_call(&e.reg, &e.exec, &e.gate, &call("fs_write", json!({"path": "notes/b.md", "content": "x"})), &ctx(&e.db, true, "3"));
    assert!(r.content.starts_with("已提交审批"), "tainted write without allow_when_tainted: {}", r.content);

    let r = dispatch_call(&e.reg, &e.exec, &e.gate, &call("shell_run", json!({"command": "echo hello"})), &ctx(&e.db, false, "4"));
    assert!(r.content.starts_with("已执行"), "{}", r.content);
    assert!(r.content.contains("hello"));
    assert_eq!(r.trust, hatch_core::Trust::Untrusted, "command output is untrusted");
    let r = dispatch_call(&e.reg, &e.exec, &e.gate, &call("shell_run", json!({"command": "echo hello"})), &ctx(&e.db, true, "5"));
    assert!(r.content.starts_with("已提交审批"), "tainted command needs the user even with allow_when_tainted: {}", r.content);
    let r = dispatch_call(&e.reg, &e.exec, &e.gate, &call("shell_run", json!({"command": "echo a; rm -rf ."})), &ctx(&e.db, false, "6"));
    assert!(r.content.starts_with("已提交审批"), "chained command is not covered: {}", r.content);
    assert!(e.dir.path().join("config/settings.ini").exists());
}

#[test]
fn secrets_and_private_urls_are_refused() {
    let e = env();
    let r = dispatch_call(&e.reg, &e.exec, &e.gate, &call("fs_write", json!({"path": "k.txt", "content": "password: hunter2hunter2"})), &ctx(&e.db, false, "1"));
    assert!(r.content.contains("被策略拒绝"), "{}", r.content);
    let r = dispatch_call(&e.reg, &e.exec, &e.gate, &call("web_fetch", json!({"url": "http://example.com"})), &ctx(&e.db, false, "2"));
    assert!(r.content.starts_with("ERROR"), "{}", r.content);
    let r = dispatch_call(&e.reg, &e.exec, &e.gate, &call("web_fetch", json!({"url": "https://169.254.169.254/latest/meta-data"})), &ctx(&e.db, false, "3"));
    assert!(r.content.contains("执行失败") || r.content.starts_with("ERROR"), "{}", r.content);
    let r = dispatch_call(&e.reg, &e.exec, &e.gate, &call("web_fetch", json!({"url": "https://example.com/"})), &ctx(&e.db, true, "4"));
    assert!(r.content.starts_with("已提交审批"), "fetch from a tainted context needs the user: {}", r.content);
}

#[test]
fn sandboxed_command_runs_in_the_workspace() {
    let e = env();
    let s = sandbox();
    if s.mode == SandboxMode::Namespaces {
        if let Err(err) = s.probe() {
            eprintln!("SKIP sandboxed_command_runs_in_the_workspace: {err}");
            return;
        }
    }
    let r = dispatch_call(&e.reg, &e.exec, &e.gate, &call("shell_run", json!({"command": "cat config/settings.ini && id -u && echo made > out.txt"})), &ctx(&e.db, false, "1"));
    let id = r.approval_id.unwrap();
    let note = approve_and_run(&e, &id);
    assert!(note.contains("timeout = 30"), "{note}");
    assert!(note.contains("exit=0"), "{note}");
    if s.mode == SandboxMode::Namespaces {
        assert!(note.contains("65534") && note.contains("沙箱隔离"), "{note}");
    }
    assert_eq!(std::fs::read_to_string(e.dir.path().join("out.txt")).unwrap(), "made\n");
    assert_eq!(e.ws.root(), e.dir.path().canonicalize().unwrap());
}
