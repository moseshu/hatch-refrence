//! The single path from "the assistant wants to do X" to "X happened".
//!
//! ```text
//! tool call ─► plan ─► Gatekeeper ─┬─ Deny ───────────────► error back to the model
//!                                  ├─ RequireApproval ────► approval queue (pending) ─► user decides later
//!                                  └─ Allow ─┬─ read/memory tool ─► Tool::call
//!                                            └─ external effect ─► approval (auto-approved, audited) ─► executor
//! ```

use crate::executors::{execute_approved_with, ExecOutcome, ExecutorRegistry};
use crate::tool::{ImageAttachment, Registry, ToolContext};
use hatch_core::{CancelToken, Error, Result, Trust};
use hatch_llm::ToolCall;
use hatch_memory::People;
use hatch_policy::{
    floor_reading, recheck_class, ActionClass, ActionRequest, ApprovalQueue, ApprovalStatus,
    Decision, FloorReading, Gatekeeper, GrantStore, NewApproval, SubmitOutcome,
};
use hatch_store::Db;
use hatch_core::time::Timestamp;

#[derive(Clone, Debug, PartialEq)]
pub enum RouteOutcome {
    /// Waiting for the user. `existed` = this exact action was already queued.
    Queued { id: String, reasons: Vec<String>, existed: bool },
    Executed { id: String, result: String },
    /// Already handled in an earlier run (executed / rejected / expired).
    AlreadyHandled { id: String, status: ApprovalStatus },
    Denied { reason: String },
    Failed { id: String, error: String },
}

/// The safety floor's verdict on a shell command, as an approval
/// reason. `None` means the command is clear: the gate's decision
/// stands. The reason names the rule, never the command itself — the
/// command may hold a credential.
fn floor_hold_reason(command: Option<&str>) -> Option<String> {
    let command = command?;
    let home = std::env::var("HOME").ok();
    match floor_reading(command, home.as_deref()) {
        FloorReading::Clear => None,
        FloorReading::Hit { rule, label } => {
            Some(format!("命中安全底线（{}）：{}，必须由你确认", rule.as_str(), label))
        }
        FloorReading::Unread => Some("命中安全底线（unreadable）：命令无法可靠解析，必须由你确认".into()),
    }
}

/// Route an external action through gate → queue → (maybe) executor.
pub fn route_action(
    db: &Db,
    gate: &Gatekeeper,
    executors: &ExecutorRegistry,
    req: &ActionRequest,
    approval: NewApproval,
    now: &Timestamp,
) -> Result<RouteOutcome> {
    route_action_with(db, gate, executors, req, approval, now, None)
}

/// [`route_action`] whose auto-executed actions honour `cancel`.
pub fn route_action_with(
    db: &Db,
    gate: &Gatekeeper,
    executors: &ExecutorRegistry,
    req: &ActionRequest,
    mut approval: NewApproval,
    now: &Timestamp,
    cancel: Option<&CancelToken>,
) -> Result<RouteOutcome> {
    let q = ApprovalQueue::new(db);
    if let Some(existing) = q.by_key(&approval.idempotency_key)? {
        // An execution that completed but reported an in-band error
        // (e.g. an MCP `isError` result) definitively did not take
        // effect, so an identical re-submission is a legitimate retry,
        // not a duplicate. Give it a distinct key and route it fresh.
        // `Failed` stays handled: its outcome may be ambiguous.
        let retryable = existing.status == ApprovalStatus::Executed
            && crate::verification::result_reports_error(&existing.note);
        if !retryable {
            return Ok(match existing.status {
                ApprovalStatus::Pending => RouteOutcome::Queued { id: existing.id, reasons: existing.reasons, existed: true },
                s => RouteOutcome::AlreadyHandled { id: existing.id, status: s },
            });
        }
        approval.idempotency_key =
            format!("{}#retry-{}", approval.idempotency_key, hatch_core::ids::random_id("r"));
    }
    let class = req.class.unwrap_or(approval.class);
    let grants = GrantStore::new(db).active_for(class, now)?;
    let mut decision = gate.evaluate(req, &grants, now);
    // Safety floor: a shell command on the never-list — or one the
    // lexer cannot read — goes to the approval queue even when a grant
    // would allow it. The floor overrides Allow before any grant is
    // consumed or anything executes; Deny still wins over the floor.
    if class == ActionClass::ExecCommand {
        if let Some(reason) = floor_hold_reason(req.recipients.first().map(|s| s.as_str())) {
            decision = match decision {
                Decision::Allow { .. } => Decision::RequireApproval { reasons: vec![reason] },
                Decision::RequireApproval { mut reasons } => {
                    reasons.insert(0, reason);
                    Decision::RequireApproval { reasons }
                }
                Decision::Deny { .. } => decision,
            };
        }
    }
    match decision {
        Decision::Deny { reason } => {
            db.audit(&approval.origin, "action_denied", &format!("{} — {reason}", approval.title))?;
            Ok(RouteOutcome::Denied { reason })
        }
        Decision::RequireApproval { reasons } => {
            approval.reasons = reasons.clone();
            let out = q.submit(&approval, now)?;
            Ok(RouteOutcome::Queued { id: out.id().to_string(), reasons, existed: matches!(out, SubmitOutcome::Existing(..)) })
        }
        Decision::Allow { grant, .. } => {
            let why = match &grant {
                Some(g) => format!("自动执行：符合常设授权 {g}"),
                None => "自动执行：该类动作的自主度允许".to_string(),
            };
            approval.reasons = vec![why.clone()];
            let id = q.submit(&approval, now)?.id().to_string();
            q.decide(&id, true, &why, now)?;
            if let Some(g) = grant {
                GrantStore::new(db).consume(&g)?;
            }
            Ok(match execute_approved_with(db, executors, &id, now, cancel)? {
                ExecOutcome::Executed(r) | ExecOutcome::AlreadyDone(r) => RouteOutcome::Executed { id, result: r },
                ExecOutcome::Ambiguous(e) | ExecOutcome::Failed(e) => RouteOutcome::Failed { id, error: e },
            })
        }
    }
}

/// Result of one tool call, ready to be returned to the model.
#[derive(Clone, Debug, PartialEq)]
pub struct DispatchResult {
    pub content: String,
    pub trust: Trust,
    pub approval_id: Option<String>,
    /// Widget payload from the tool, if any (see `ToolOutput::widget`).
    /// The chat agent ignores it (text fallback is the content); the MCP
    /// server copies it into `structuredContent.widget`.
    pub widget: Option<serde_json::Value>,
    /// Image attached to the result, if any (see `ToolOutput::image`).
    /// Only tools that produce pictures set it; the agent loop carries
    /// it into the next model call.
    pub image: Option<ImageAttachment>,
}

fn unknown_recipients(db: &Db, recipients: &[String], user_email: &str) -> Result<Vec<String>> {
    let people = People::new(db);
    let mut out = Vec::new();
    for r in recipients {
        if !r.eq_ignore_ascii_case(user_email) && !people.is_known(r)? {
            out.push(r.clone());
        }
    }
    Ok(out)
}

/// Render a dispatch failure as `ERROR: [kind] msg`. The kind tag
/// (from `Error::kind`) is what the model and the loop's streak
/// detector key on: structural kinds carry fixed guidance that
/// retrying with different arguments cannot help, so the model
/// stops hammering the same wall and reports the block instead.
/// The `ERROR: ` prefix is load-bearing — plan execution, the MCP
/// server and the completion gate all recognise errors by it.
fn err_kind(kind: &str, m: String) -> DispatchResult {
    let guidance = match kind {
        "denied" | "sandbox" | "secret_rejected" | "config" | "quota_exceeded" => {
            "。环境限制：换参数重试无效，请停止对该目标的尝试，向用户汇报阻塞原因与所需条件。"
        }
        "unknown_outcome" => "。结果未知：不要自动重试，等待人工对账。",
        _ => "",
    };
    DispatchResult {
        content: format!("ERROR: [{kind}] {m}{guidance}"),
        trust: Trust::System,
        approval_id: None,
        widget: None,
        image: None,
    }
}

fn err_from(e: &Error) -> DispatchResult {
    err_kind(e.kind(), e.to_string())
}

/// Dispatch one model tool call. Never blocks on the user.
pub fn dispatch_call(
    reg: &Registry,
    executors: &ExecutorRegistry,
    gate: &Gatekeeper,
    call: &ToolCall,
    ctx: &ToolContext,
) -> DispatchResult {
    let Some(tool) = reg.get(&call.name) else {
        return err_kind("invalid", format!("unknown tool `{}`; available: {}", call.name, reg.names().join(", ")));
    };
    let plan = match tool.plan(&call.arguments, ctx) {
        Ok(p) => p,
        Err(e) => return err_from(&e),
    };
    let class = plan.class.unwrap_or_else(|| tool.class());
    // P0 #5: the plan class is the tool's own claim about its effect —
    // or a third-party manifest's, for MCP/connector tools — so a name
    // that says "send" outranks a quiet class. `recheck_class` only
    // upgrades to SendMessage; a class that already needs the user keeps
    // its own protections (the safety floor keys on ExecCommand).
    let class = recheck_class(&call.name, class);
    let req = ActionRequest {
        class: Some(class),
        summary: plan.summary.clone(),
        unknown_recipients: unknown_recipients(ctx.db, &plan.recipients, &ctx.user_email).unwrap_or_default(),
        recipients: plan.recipients.clone(),
        labels: plan.labels.clone(),
        tainted: ctx.tainted,
    };
    let deferred = match tool.to_approval(&call.arguments, ctx) {
        Ok(d) => d,
        Err(e) => return err_from(&e),
    };
    match deferred {
        Some(approval) => {
            let result_trust =
                if crate::ws_exec::UNTRUSTED_RESULT_KINDS.contains(&approval.kind.as_str()) { Trust::Untrusted } else { Trust::System };
            match route_action_with(ctx.db, gate, executors, &req, approval, &ctx.now, Some(&ctx.cancel)) {
            Ok(RouteOutcome::Queued { id, reasons, .. }) => DispatchResult {
                content: format!(
                    "已提交审批 {id}，尚未执行。需要用户确认的原因：{}。请告诉用户可用 `hatch approvals show {id}` 查看并批准。",
                    reasons.join("；")
                ),
                trust: Trust::System,
                approval_id: Some(id),
                widget: None,
                image: None,
            },
            Ok(RouteOutcome::Executed { id, result }) => {
                DispatchResult { content: format!("已执行（{id}）：{result}"), trust: result_trust, approval_id: Some(id), widget: None, image: None }
            }
            Ok(RouteOutcome::AlreadyHandled { id, status }) => DispatchResult {
                content: format!("相同动作此前已处理（{id}，状态 {}），未重复执行。", status.as_str()),
                trust: Trust::System,
                approval_id: Some(id),
                widget: None,
                image: None,
            },
            Ok(RouteOutcome::Denied { reason }) => err_kind("denied", format!("被策略拒绝：{reason}")),
            Ok(RouteOutcome::Failed { id, error }) => err_kind("error", format!("执行失败（{id}）：{error}")),
            Err(e) => err_from(&e),
            }
        }
        None => {
            let grants = GrantStore::new(ctx.db).active_for(class, &ctx.now).unwrap_or_default();
            match gate.evaluate(&req, &grants, &ctx.now) {
                Decision::Allow { .. } => match tool.call(&call.arguments, ctx) {
                    Ok(o) => DispatchResult { content: o.content, trust: o.trust, approval_id: None, widget: o.widget, image: o.image },
                    Err(e) => err_from(&e),
                },
                Decision::RequireApproval { reasons } => err_kind("denied", format!("此操作需要用户确认但无法排队：{}", reasons.join("；"))),
                Decision::Deny { reason } => err_kind("denied", format!("被策略拒绝：{reason}")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::{add_general_tools, default_registry, GeneralTools};
    use crate::executors::{ActionExecutor, CreateEventExecutor, ExecContext, SendEmailExecutor};
    use crate::workspace::Workspace;
    use hatch_connectors::mail::fixture::FileOutbox;
    use hatch_policy::{ActionClass, NewGrant, PolicyConfig};
    use serde_json::{json, Value};
    use std::sync::Arc;

    struct Env {
        db: Db,
        _dir: tempfile::TempDir,
        outbox: Arc<FileOutbox>,
        exec: ExecutorRegistry,
        gate: Gatekeeper,
    }

    fn env() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let outbox = Arc::new(FileOutbox::new(dir.path(), "me@126.com").unwrap());
        Env {
            db: Db::open_in_memory().unwrap(),
            exec: ExecutorRegistry::new()
                .with(Arc::new(SendEmailExecutor::new(outbox.clone(), "me@126.com")))
                .with(Arc::new(CreateEventExecutor)),
            outbox,
            _dir: dir,
            gate: Gatekeeper::new(PolicyConfig::default()),
        }
    }

    fn ctx(db: &Db, tainted: bool) -> ToolContext<'_> {
        ToolContext { db, now: hatch_core::time::now(), tz: chrono_tz::Asia::Shanghai, user_email: "me@126.com".into(), user_name: String::new(), origin: "chat:s1".into(), tainted, call_id: "c1".into(), cancel: CancelToken::new() }
    }

    fn email(to: &str, body: &str) -> ToolCall {
        ToolCall { id: "c".into(), name: "propose_email".into(), arguments: json!({"to": [to], "subject": "周报", "body": body}) }
    }

    #[test]
    fn email_is_queued_not_sent() {
        let e = env();
        let r = dispatch_call(&default_registry(), &e.exec, &e.gate, &email("boss@corp.com", "周报见附件"), &ctx(&e.db, false));
        assert!(r.content.starts_with("已提交审批"), "{}", r.content);
        assert!(r.content.contains("收件人不在联系人中"));
        assert_eq!(e.outbox.entries().unwrap().len(), 0);
        let again = dispatch_call(&default_registry(), &e.exec, &e.gate, &email("boss@corp.com", "周报见附件"), &ctx(&e.db, false));
        assert_eq!(again.approval_id, r.approval_id, "same call maps to the same approval");
        assert_eq!(ApprovalQueue::new(&e.db).list(None).unwrap().len(), 1);
    }

    #[test]
    fn grant_executes_but_not_from_tainted_context() {
        let e = env();
        let now = hatch_core::time::now();
        GrantStore::new(&e.db)
            .add(&NewGrant { class: ActionClass::SendMessage, recipients: vec!["*@corp.com".into()], max_uses: None, allow_when_tainted: false, expires_at: None, note: String::new() }, &now)
            .unwrap();
        let r = dispatch_call(&default_registry(), &e.exec, &e.gate, &email("boss@corp.com", "收到"), &ctx(&e.db, false));
        assert!(r.content.starts_with("已执行"), "{}", r.content);
        assert_eq!(e.outbox.entries().unwrap().len(), 1);
        let r = dispatch_call(&default_registry(), &e.exec, &e.gate, &email("boss@corp.com", "另一封"), &ctx(&e.db, true));
        assert!(r.content.starts_with("已提交审批"), "{}", r.content);
        assert_eq!(e.outbox.entries().unwrap().len(), 1);
    }

    #[test]
    fn secrets_and_unknown_tools_are_refused() {
        let e = env();
        let r = dispatch_call(&default_registry(), &e.exec, &e.gate, &email("a@corp.com", "我的密码：hunter22x"), &ctx(&e.db, false));
        assert!(r.content.contains("被策略拒绝"), "{}", r.content);
        let r = dispatch_call(&default_registry(), &e.exec, &e.gate, &ToolCall { id: "x".into(), name: "rm_rf".into(), arguments: json!({}) }, &ctx(&e.db, false));
        assert!(r.content.contains("unknown tool"));
        let r = dispatch_call(&default_registry(), &e.exec, &e.gate, &ToolCall { id: "x".into(), name: "memory_search".into(), arguments: json!({"query": "x"}) }, &ctx(&e.db, false));
        assert_eq!(r.content, "No relevant memory found.");
    }

    #[test]
    fn event_proposal_queues_with_preview() {
        let e = env();
        let call = ToolCall { id: "e".into(), name: "propose_event".into(), arguments: json!({"title": "项目评审", "start": "2026-11-05 15:00"}) };
        let r = dispatch_call(&default_registry(), &e.exec, &e.gate, &call, &ctx(&e.db, false));
        let a = ApprovalQueue::new(&e.db).get(r.approval_id.as_deref().unwrap()).unwrap();
        assert!(a.preview.contains("2026-11-05 15:00"));
        assert_eq!(a.kind, "create_event");
    }

    /// A shell executor that records execution without running anything.
    struct StubShellExec;

    impl ActionExecutor for StubShellExec {
        fn kind(&self) -> &'static str {
            "exec_command"
        }

        fn execute(&self, _payload: &Value, _ctx: &ExecContext) -> Result<String> {
            Ok("stub-ok".into())
        }
    }

    fn shell_setup() -> (Db, tempfile::TempDir, Registry, ExecutorRegistry, Gatekeeper) {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        let reg = add_general_tools(
            default_registry(),
            &GeneralTools { workspace: Some(ws), web_fetch: false, search_backend: None, max_command_timeout_secs: 60 },
        );
        let exec = ExecutorRegistry::new().with(Arc::new(StubShellExec));
        (Db::open_in_memory().unwrap(), dir, reg, exec, Gatekeeper::new(PolicyConfig::default()))
    }

    fn shell_call(command: &str) -> ToolCall {
        ToolCall { id: "c".into(), name: "shell_run".into(), arguments: json!({"command": command}) }
    }

    fn permissive_exec_grant(db: &Db) {
        let now = hatch_core::time::now();
        GrantStore::new(db)
            .add(
                &NewGrant { class: ActionClass::ExecCommand, recipients: vec!["*".into()], max_uses: None, allow_when_tainted: false, expires_at: None, note: String::new() },
                &now,
            )
            .unwrap();
    }


    #[test]
    fn safety_floor_beats_a_permissive_grant() {
        let (db, _dir, reg, exec, gate) = shell_setup();
        permissive_exec_grant(&db);
        let r = dispatch_call(&reg, &exec, &gate, &shell_call("rm -rf /"), &ctx(&db, false));
        assert!(r.content.starts_with("已提交审批"), "{}", r.content);
        assert!(r.content.contains("安全底线"), "{}", r.content);
        let approvals = ApprovalQueue::new(&db).list(None).unwrap();
        assert_eq!(approvals.len(), 1);
        assert_eq!(approvals[0].status, ApprovalStatus::Pending);
        assert!(approvals[0].reasons.iter().any(|x| x.contains("安全底线")), "{:?}", approvals[0].reasons);
        // The grant was not consumed: it is still there for a later command.
        assert_eq!(GrantStore::new(&db).list().unwrap()[0].used, 0);
    }

    #[test]
    fn benign_command_with_grant_still_executes() {
        let (db, _dir, reg, exec, gate) = shell_setup();
        permissive_exec_grant(&db);
        let r = dispatch_call(&reg, &exec, &gate, &shell_call("rm -rf ./build"), &ctx(&db, false));
        assert!(r.content.starts_with("已执行"), "{}", r.content);
        assert!(r.content.contains("stub-ok"), "{}", r.content);
    }

    #[test]
    fn unreadable_command_with_grant_goes_to_approval() {
        let (db, _dir, reg, exec, gate) = shell_setup();
        permissive_exec_grant(&db);
        let r = dispatch_call(&reg, &exec, &gate, &shell_call("echo \"unterminated"), &ctx(&db, false));
        assert!(r.content.starts_with("已提交审批"), "{}", r.content);
        assert!(r.content.contains("安全底线"), "{}", r.content);
    }

    #[test]
    fn errors_carry_kind_tag_and_env_guidance() {
        // Direct unit checks on the renderer: structural kinds get
        // the stop-retrying guidance, fixable kinds do not.
        let denied = err_kind("denied", "policy denied: x".into());
        assert!(denied.content.starts_with("ERROR: [denied] "), "{}", denied.content);
        assert!(denied.content.contains("环境限制"), "{}", denied.content);
        let invalid = err_kind("invalid", "bad args".into());
        assert!(invalid.content.starts_with("ERROR: [invalid] "), "{}", invalid.content);
        assert!(!invalid.content.contains("环境限制"), "{}", invalid.content);
        let unknown = err_kind("unknown_outcome", "timeout after send".into());
        assert!(unknown.content.contains("不要自动重试"), "{}", unknown.content);
        // err_from maps the structured kind through.
        let from = err_from(&Error::NotFound("gone".into()));
        assert!(from.content.starts_with("ERROR: [not_found] "), "{}", from.content);
    }

    #[test]
    fn unknown_tool_error_is_tagged_invalid() {
        let e = env();
        let reg = default_registry();
        let call = ToolCall { id: "c1".into(), name: "nope_tool".into(), arguments: json!({}) };
        let r = dispatch_call(&reg, &e.exec, &e.gate, &call, &ctx(&e.db, false));
        assert!(r.content.starts_with("ERROR: [invalid] unknown tool"), "{}", r.content);
    }
}
