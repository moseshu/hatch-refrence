//! P1 acceptance drills (a) + (b): natural language submitted in
//! chat becomes a durable task the resident worker runs — no CLI
//! run, no manual API call. Script model + stub Gmail only; no real
//! model, no real accounts.
//!
//! (a) meeting-prep: "给我准备明天和李婷的会，看看最近邮件里的
//! 决定" → meeting-brief task → resident runs → brief file with
//! sources, after the artifact approval.
//! (b) calendar invite: natural language → meeting-invite task
//! (gmail source, stub helper) → local calendar event id + exactly
//! one stub Gmail send, through plan / create-event / send
//! approvals + auto-resume.

use crate::app::App;
use crate::commands::approval_service::ApprovalService;
use crate::commands::chat::setup_turn;
use crate::commands::task_list_drill::{approve_write, artifact_approval_id};
use crate::commands::task_resident::ResidentWorker;
use chrono::TimeZone;
use hatch_agent::{Agent, AgentConfig, RunOptions, SessionStore, TurnReport};
use hatch_core::time::now;
use hatch_core::CancelToken;
use hatch_eval::World;
use hatch_llm::scripted::FnModel;
use hatch_llm::{ChatResponse, Router, ToolCall};
use hatch_policy::ApprovalStatus;
use hatch_store::Db;
use hatch_tasks::{TaskApprovals, TaskStatus, TaskStore, UnitStore};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn world_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../eval/worlds/meeting-brief")
}

/// A fresh home with a long resident sweep interval, so the worker
/// only runs when the drill sweeps it.
pub(crate) fn setup() -> (Arc<App>, TempDir) {
    let home = TempDir::new().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        "[tasks]\nresident_sweep_secs = 3600\n[user]\nemail = \"moses@gmail.example\"\n",
    )
    .unwrap();
    let app = Arc::new(App::open(Some(home.path())).unwrap());
    (app, home)
}

/// Run one chat turn whose scripted model submits `args` through the
/// `submit_goal` tool on its first model call, then answers in text.
/// Returns the turn report.
pub(crate) fn run_chat_submit(app: &Arc<App>, user_text: &str, args: Value) -> TurnReport {
    let setup = setup_turn(app, None).unwrap();
    let counter = Arc::new(AtomicUsize::new(0));
    let model = FnModel::new("chat-script", move |_req| {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            Ok(ChatResponse::calls(vec![ToolCall {
                id: "c-submit".into(),
                name: "submit_goal".into(),
                arguments: args.clone(),
            }]))
        } else {
            Ok(ChatResponse::text("已提交，任务在后台跑。"))
        }
    });
    let router = Router::single(Arc::new(model));
    let agent = Agent {
        db: &app.db,
        router: &router,
        tools: &setup.tools,
        executors: &setup.executors,
        gate: &setup.gate,
        config: AgentConfig {
            user_name: app.config.user.name.clone(),
            user_email: app.user_email(),
            tz: app.tz,
            delegation: setup.delegation,
            skills: setup.skill_bodies,
            skill_tools: setup.skill_tools,
            ..Default::default()
        },
    };
    let session = SessionStore::new(&app.db)
        .create("p1-drill", &now())
        .unwrap();
    agent
        .run(
            &session,
            user_text,
            &now(),
            RunOptions {
                on_delta: None,
                cancel: CancelToken::new(),
                observer: None,
                on_step_event: None,
                images: Vec::new(),
            },
        )
        .unwrap()
}

pub(crate) fn status(db: &Db, id: &str) -> TaskStatus {
    TaskStore::new(db).get(id).unwrap().unwrap().status
}

pub(crate) fn only_task(app: &Arc<App>) -> String {
    let tasks = TaskStore::new(&app.db).list(None, 10).unwrap();
    assert_eq!(tasks.len(), 1, "提交后应恰好有一个任务");
    tasks[0].id.clone()
}

/// Sweep until the task reaches `want` (or the deadline).
pub(crate) fn sweep_until(app: &Arc<App>, worker: &mut ResidentWorker, id: &str, want: TaskStatus, timeout_ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if status(&app.db, id) == want {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        let _ = worker.sweep_once();
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// What the task actually looks like right now — status, error,
/// blocked kind, and every step's status/output — so a sweep
/// timeout fails with something actionable instead of a bare
/// "should be Blocked". Only evaluated when an assertion fails
/// (assert message args are lazy).
pub(crate) fn sweep_detail(app: &Arc<App>, task_id: &str) -> String {
    let task = match TaskStore::new(&app.db).get(task_id) {
        Ok(Some(t)) => t,
        Ok(None) => return format!("任务 {task_id} 不存在"),
        Err(e) => return format!("读任务 {task_id} 失败：{e:#}"),
    };
    let mut s = format!(
        "任务 {}：状态 {:?}，错误：{}，阻塞原因：{:?}",
        task.id, task.status, task.error, task.blocked_kind
    );
    match UnitStore::new(&app.db).list_steps(task_id) {
        Ok(steps) => {
            for st in steps {
                s.push_str(&format!(
                    "\n  步骤 {}（{}）：{:?}，失败分类：{}，输出：{}",
                    st.step_id, st.title, st.status, st.failure_kind, st.output
                ));
            }
        }
        Err(e) => s.push_str(&format!("\n读步骤失败：{e:#}")),
    }
    s
}

/// Approve every pending approval linked to the task, through the
/// shared service (plan / create-event / send / write all go
/// through it, so the right executor runs for each).
fn approve_pending(app: &Arc<App>, task_id: &str) {
    let svc = ApprovalService::new(app);
    let links = TaskApprovals::new(&app.db).for_task(task_id).unwrap();
    for link in links {
        let current = hatch_policy::ApprovalQueue::new(&app.db)
            .get(&link.approval_id)
            .unwrap()
            .status;
        if current == ApprovalStatus::Pending {
            let _ = svc.approve_and_execute(&link.approval_id, "drill approve", false, &now());
        }
    }
}

/// Drive the task to Done: sweep, approve whatever blocks it,
/// repeat. Returns whether it got there.
pub(crate) fn drive_to_done(app: &Arc<App>, worker: &mut ResidentWorker, task_id: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if status(&app.db, task_id) == TaskStatus::Done {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        let _ = worker.sweep_once();
        if status(&app.db, task_id) == TaskStatus::Blocked {
            approve_pending(app, task_id);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn chat_submit_meeting_brief_runs_to_done() {
    let (app, _home) = setup();
    World::load(&world_dir()).unwrap();
    let report = run_chat_submit(
        &app,
        "给我准备明天和李婷的会，看看最近邮件里的决定",
        json!({
            "goal": "给我准备明天和李婷的会，看看最近邮件里的决定",
            "kind": "meeting-brief",
            "world": world_dir().to_string_lossy(),
            "source": "fixture",
            "acceptance": ["简报引用每条决定的来源邮件"],
        }),
    );
    assert_eq!(report.stop, hatch_agent::StopReason::Answered);
    let task_id = only_task(&app);
    let task = TaskStore::new(&app.db).get(&task_id).unwrap().unwrap();
    assert_eq!(task.kind, "meeting-brief");
    // The resident worker runs it with no CLI run and no API call:
    // triage + gather finish, then it blocks on the artifact
    // approval.
    let mut worker = ResidentWorker::new(&app);
    assert!(
        sweep_until(&app, &mut worker, &task_id, TaskStatus::Blocked, 30_000),
        "任务应在 artifact 审批上阻塞；{}",
        sweep_detail(&app, &task_id)
    );
    approve_write(&app.db, &artifact_approval_id(&app.db, &task_id), now());
    assert!(
        sweep_until(&app, &mut worker, &task_id, TaskStatus::Done, 30_000),
        "批准后任务应完成；{}",
        sweep_detail(&app, &task_id)
    );
    // The brief exists and cites its sources.
    let out_dir = app.home.join("task-out").join(&task_id);
    let brief = std::fs::read_to_string(out_dir.join("brief-v1.md")).unwrap();
    assert!(brief.contains("李婷"), "简报应提到李婷：\n{brief}");
    assert!(brief.contains("m201"), "简报应引用来源邮件：\n{brief}");
    assert!(brief.contains("m202"), "简报应引用来源邮件：\n{brief}");
}

/// The stub Gmail helper (same shape as `m2-gmail-e2e.sh`): +triage
/// returns two canned messages, +read their bodies, +send logs the
/// send and returns a canned id. Anything else fails loudly.
const STUB: &str = r#"#!/usr/bin/env bash
echo "$*" >> "$CALL_LOG"
case "$2" in
  +triage)
    cat <<'JSON'
[{"id":"msg-1","subject":"明天评审会：方案 B 通过","from":"李婷 <li.ting@corp.example>","snippet":"方案 B 通过"},{"id":"msg-2","subject":"Re: 明天评审会：预算确认","from":"李婷 <li.ting@corp.example>","snippet":"预算上限 5 万元"}]
JSON
    ;;
  +read)
    id=""; prev=""
    for a in "$@"; do if [ "$prev" = "--id" ]; then id="$a"; fi; prev="$a"; done
    case "$id" in
      msg-1) echo '{"id":"msg-1","headers":{"From":"李婷 <li.ting@corp.example>","To":"moses@gmail.example","Subject":"明天评审会：方案 B 通过","Date":"Fri, 13 Nov 2026 10:00:00 +0800"},"body_text":"方案 B 通过。"}' ;;
      msg-2) echo '{"id":"msg-2","headers":{"From":"李婷 <li.ting@corp.example>","To":"moses@gmail.example","Subject":"Re: 明天评审会：预算确认","Date":"Fri, 13 Nov 2026 09:00:00 +0800"},"body_text":"预算上限 5 万元。"}' ;;
      *) echo "stub: unknown message id $id" >&2; exit 1;;
    esac
    ;;
  +send)
    echo "$*" >> "$SEND_LOG"
    # Save the sent mail so the verification read-back
    # (`users messages get`) can return it.
    to=""; subject=""; body=""; from=""; prev=""
    for a in "$@"; do
      case "$prev" in
        --to) to="$a";; --subject) subject="$a";; --body) body="$a";; --from) from="$a";;
      esac
      prev="$a"
    done
    printf '%s' "$from" > "$SEND_LOG.from"
    printf '%s' "$to" > "$SEND_LOG.to"
    printf '%s' "$subject" > "$SEND_LOG.subject"
    printf '%s' "$body" > "$SEND_LOG.body"
    echo '{"id":"stub-send-1","message_id":"stub-send-1"}'
    ;;
  users)
    # gmail users messages get --params {"userId":"me","id":"stub-send-1","format":"raw"}
    from=$(cat "$SEND_LOG.from"); to=$(cat "$SEND_LOG.to"); subject=$(cat "$SEND_LOG.subject")
    subj_b64=$(printf '%s' "$subject" | base64 | tr -d '\n')
    body_b64=$(base64 < "$SEND_LOG.body" | tr -d '\n')
    raw=$(printf 'From: %s\r\nTo: %s\r\nSubject: =?UTF-8?B?%s?=\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=UTF-8\r\nContent-Transfer-Encoding: base64\r\n\r\n%s\r\n' "$from" "$to" "$subj_b64" "$body_b64" | base64 | tr '+/' '-_' | tr -d '=\n')
    printf '{"id":"stub-send-1","labelIds":["SENT"],"raw":"%s"}\n' "$raw"
    ;;
  *) echo "stub: unexpected call: $*" >&2; exit 1;;
esac
"#;

static ENV_OVERRIDE_LOCK: Mutex<()> = Mutex::new(());

/// Serialize test overrides of process-wide settings and restore them on drop.
/// Acquire only one guard per test scope, passing every override in that call.
pub(crate) struct EnvGuard {
    old: Vec<(&'static str, Option<std::ffi::OsString>)>,
    _lock: MutexGuard<'static, ()>,
}

impl EnvGuard {
    pub(crate) fn set(pairs: &[(&'static str, &std::path::Path)]) -> EnvGuard {
        let lock = ENV_OVERRIDE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut old = Vec::new();
        for (key, value) in pairs {
            old.push((*key, std::env::var_os(key)));
            std::env::set_var(key, value);
        }
        EnvGuard { old, _lock: lock }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in &self.old {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[test]
fn chat_submit_invite_sends_once_via_stub_gmail() {
    // The stub helper is selected through the process environment
    // (the same `HATCH_GWS_CLI` override the E2E scripts use).
    let work = TempDir::new().unwrap();
    let stub_path = work.path().join("hatch_gws_cli");
    std::fs::write(&stub_path, STUB).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let call_log = work.path().join("calls.log");
    let send_log = work.path().join("sends.log");
    let _guard = EnvGuard::set(&[
        ("HATCH_GWS_CLI", &stub_path),
        ("CALL_LOG", &call_log),
        ("SEND_LOG", &send_log),
    ]);
    invite_flow_inner();
}

fn invite_flow_inner() {
    let (app, _home) = setup();
    // Tomorrow 10:00 in the account timezone, in the format the
    // invite spec takes.
    let tz: chrono_tz::Tz = "Asia/Shanghai".parse().unwrap();
    let start = tz.from_utc_datetime(&(now() + chrono::Duration::days(1)).naive_utc());
    let event_start = start.format("%Y-%m-%d %H:%M").to_string();
    let report = run_chat_submit(
        &app,
        "给李婷发明天评审会的邀请，顺便在本地日历上建个日程",
        json!({
            "goal": "给李婷发明天评审会的邀请，顺便在本地日历上建个日程",
            "kind": "meeting-invite",
            "source": "gmail",
            "gmail_user": "moses@gmail.example",
            "gmail_name": "Moses",
            "invite": {
                "event_title": "明天评审会",
                "event_start": event_start,
                "attendee_email": "li.ting@corp.example",
                "attendee_name": "李婷",
                "topics": "方案 B",
                "create_event": true,
            },
        }),
    );
    assert_eq!(report.stop, hatch_agent::StopReason::Answered);
    let task_id = only_task(&app);
    let task = TaskStore::new(&app.db).get(&task_id).unwrap().unwrap();
    assert_eq!(task.kind, "meeting-invite");
    let mut worker = ResidentWorker::new(&app);
    assert!(
        drive_to_done(&app, &mut worker, &task_id),
        "邀请任务应在审批流转后完成；{}",
        sweep_detail(&app, &task_id)
    );
    // Local calendar event: the create-event step's output carries
    // the stable event id.
    let steps = UnitStore::new(&app.db).list_steps(&task_id).unwrap();
    let create = steps.iter().find(|s| s.tool == "propose_event").expect("应有建日程步骤");
    assert_eq!(create.status, hatch_tasks::ExecStatus::Done);
    assert!(create.output.contains("evt_"), "建日程输出应含事件 id：{}", create.output);
    // Exactly one stub Gmail send (one send spans several lines —
    // the body has newlines — so count the `+send` invocations).
    let sends = std::fs::read_to_string(
        std::env::var("SEND_LOG").unwrap(),
    )
    .unwrap();
    let send_calls = sends.lines().filter(|l| l.starts_with("gmail +send")).count();
    assert_eq!(send_calls, 1, "应恰好发送一次：\n{sends}");
    assert!(sends.contains("li.ting@corp.example"), "发送对象应为李婷：\n{sends}");
}
