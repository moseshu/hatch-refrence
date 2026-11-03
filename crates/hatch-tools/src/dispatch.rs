//! The single path from "the assistant wants to do X" to "X happened".
//!
//! ```text
//! tool call ─► plan ─► Gatekeeper ─┬─ Deny ───────────────► error back to the model
//!                                  ├─ RequireApproval ────► approval queue (pending) ─► user decides later
//!                                  └─ Allow ─┬─ read/memory tool ─► Tool::call
//!                                            └─ external effect ─► approval (auto-approved, audited) ─► executor
//! ```

use crate::executors::{execute_approved_with, ExecOutcome, ExecutorRegistry};
use crate::tool::{Registry, ToolContext};
use hatch_core::{CancelToken, Result, Trust};
use hatch_llm::ToolCall;
use hatch_memory::People;
use hatch_policy::{ActionRequest, ApprovalQueue, ApprovalStatus, Decision, Gatekeeper, GrantStore, NewApproval, SubmitOutcome};
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
        return Ok(match existing.status {
            ApprovalStatus::Pending => RouteOutcome::Queued { id: existing.id, reasons: existing.reasons, existed: true },
            s => RouteOutcome::AlreadyHandled { id: existing.id, status: s },
        });
    }
    let class = req.class.unwrap_or(approval.class);
    let grants = GrantStore::new(db).active_for(class, now)?;
    match gate.evaluate(req, &grants, now) {
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

/// Dispatch one model tool call. Never blocks on the user.
pub fn dispatch_call(
    reg: &Registry,
    executors: &ExecutorRegistry,
    gate: &Gatekeeper,
    call: &ToolCall,
    ctx: &ToolContext,
) -> DispatchResult {
    let err = |m: String| DispatchResult { content: format!("ERROR: {m}"), trust: Trust::System, approval_id: None };
    let Some(tool) = reg.get(&call.name) else {
        return err(format!("unknown tool `{}`; available: {}", call.name, reg.names().join(", ")));
    };
    let plan = match tool.plan(&call.arguments, ctx) {
        Ok(p) => p,
        Err(e) => return err(e.to_string()),
    };
    let req = ActionRequest {
        class: Some(tool.class()),
        summary: plan.summary.clone(),
        unknown_recipients: unknown_recipients(ctx.db, &plan.recipients, &ctx.user_email).unwrap_or_default(),
        recipients: plan.recipients.clone(),
        labels: plan.labels.clone(),
        tainted: ctx.tainted,
    };
    let deferred = match tool.to_approval(&call.arguments, ctx) {
        Ok(d) => d,
        Err(e) => return err(e.to_string()),
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
            },
            Ok(RouteOutcome::Executed { id, result }) => {
                DispatchResult { content: format!("已执行（{id}）：{result}"), trust: result_trust, approval_id: Some(id) }
            }
            Ok(RouteOutcome::AlreadyHandled { id, status }) => DispatchResult {
                content: format!("相同动作此前已处理（{id}，状态 {}），未重复执行。", status.as_str()),
                trust: Trust::System,
                approval_id: Some(id),
            },
            Ok(RouteOutcome::Denied { reason }) => err(format!("被策略拒绝：{reason}")),
            Ok(RouteOutcome::Failed { id, error }) => err(format!("执行失败（{id}）：{error}")),
            Err(e) => err(e.to_string()),
            }
        }
        None => {
            let grants = GrantStore::new(ctx.db).active_for(tool.class(), &ctx.now).unwrap_or_default();
            match gate.evaluate(&req, &grants, &ctx.now) {
                Decision::Allow { .. } => match tool.call(&call.arguments, ctx) {
                    Ok(o) => DispatchResult { content: o.content, trust: o.trust, approval_id: None },
                    Err(e) => err(e.to_string()),
                },
                Decision::RequireApproval { reasons } => err(format!("此操作需要用户确认但无法排队：{}", reasons.join("；"))),
                Decision::Deny { reason } => err(format!("被策略拒绝：{reason}")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::default_registry;
    use crate::executors::{CreateEventExecutor, SendEmailExecutor};
    use hatch_connectors::mail::fixture::FileOutbox;
    use hatch_policy::{ActionClass, NewGrant, PolicyConfig};
    use serde_json::json;
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
        ToolContext { db, now: hatch_core::time::now(), tz: chrono_tz::Asia::Shanghai, user_email: "me@126.com".into(), origin: "chat:s1".into(), tainted, call_id: "c1".into(), cancel: CancelToken::new() }
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
        assert_eq!(r.content, "没有找到相关记忆。");
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
}
