//! Scope of scheduled outbound actions: a triage_sync job with an empty scope
//! must queue its reply for approval even though a standing grant (that also
//! allows tainted contexts) covers the recipient. A control job whose scope
//! includes `send_message` shows the same grant would otherwise auto-send.

use super::{ts, SchedulerWorld};
use crate::report::Check;
use hatch_connectors::mail::fixture::{FileOutbox, FixtureMailbox};
use hatch_core::{CancelToken, Result};
use hatch_llm::scripted::FnModel;
use hatch_llm::{ChatResponse, LlmError, Router, Usage};
use hatch_policy::{
    ActionClass, ApprovalQueue, AttentionPolicy, GrantStore, NewGrant, PolicyConfig,
};
use hatch_scheduler::{Engine, JobStore, NewJob, RunStatus};
use hatch_store::Db;
use hatch_tools::{CreateEventExecutor, ExecutorRegistry, SendEmailExecutor};
use hatch_triage::TriageConfig;
use hatch_workers::triage_sync::{SourceFactory, KIND};
use hatch_workers::{RouterFactory, TriageSyncWorker};
use serde::Serialize;
use serde_json::json;
use std::sync::Arc;

fn check(name: &str, passed: bool, detail: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        passed,
        detail: detail.into(),
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Arm {
    pub scope: Vec<String>,
    pub run_status: String,
    pub run_summary: String,
    pub pending_approvals: usize,
    pub reasons: Vec<String>,
    pub outbox: usize,
    pub model_calls: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct OutboundOutcome {
    pub scoped: Arm,
    pub control: Arm,
}

/// Pipeline model for triage: classify as needing a reply, draft a short body.
fn oracle() -> RouterFactory {
    let model = Arc::new(FnModel::new("triage-oracle", |req| {
        let system = req
            .messages
            .first()
            .map(|m| m.content.as_str())
            .unwrap_or("");
        let text = if system.contains("分诊分类器") {
            json!({"category": "reply", "priority": "normal", "summary": "询问试点能否 12 月第一周启动", "needs_reply": true,
                   "reply_hint": "确认启动时间", "events": [], "suspicious": false, "suspicious_reason": ""})
        } else if system.contains("起草") {
            json!({"body": "Alice 你好，12 月第一周可以启动试点，我们按这个时间准备。"})
        } else {
            return Err(LlmError::permanent("oracle: unsupported request"));
        };
        Ok(ChatResponse {
            content: Some(text.to_string()),
            usage: Usage {
                prompt_tokens: 400,
                completion_tokens: 60,
                ..Default::default()
            },
            model: "triage-oracle".into(),
            ..Default::default()
        })
    }));
    Arc::new(move || Ok(Router::single(model.clone())))
}

fn arm(world: &SchedulerWorld, factory: RouterFactory, scope: Vec<String>) -> Result<Arm> {
    let o = &world.spec.outbound;
    let me = world.spec.persona.email.clone();
    let now = ts(&o.mail.date)?;
    let db = Db::open_in_memory()?;
    let dir = tempfile::tempdir()?;
    let outbox = Arc::new(FileOutbox::new(dir.path(), &me)?);
    let executors = ExecutorRegistry::new()
        .with(Arc::new(SendEmailExecutor::new(outbox.clone(), &me)))
        .with(Arc::new(CreateEventExecutor));
    GrantStore::new(&db).add(
        &NewGrant {
            class: ActionClass::SendMessage,
            recipients: vec![o.grant_pattern.clone()],
            max_uses: None,
            allow_when_tainted: true,
            expires_at: None,
            note: "评估：覆盖供应商的常设授权".into(),
        },
        &now,
    )?;
    let (mail, account) = (o.mail.clone(), me.clone());
    let source: SourceFactory = Arc::new(move || {
        Ok(Box::new(FixtureMailbox::new(&account, vec![mail.clone()]))
            as Box<dyn hatch_connectors::MailSource>)
    });
    let attention = AttentionPolicy {
        tz: world.tz,
        ..Default::default()
    };
    let worker = TriageSyncWorker {
        router: Some(factory),
        policy: PolicyConfig::default(),
        executors: Arc::new(executors),
        source,
        config: TriageConfig {
            user_name: world.spec.persona.name.clone(),
            user_email: me,
            tz: world.tz,
            fetch_limit: 10,
            approval_ttl_hours: 72,
        },
        attention,
    };
    let e = Engine::new(&db, world.tz).register(KIND, Arc::new(worker));
    let mut job = NewJob::new("triage-sync", KIND, "every 30m");
    job.scope = scope.clone();
    JobStore::new(&db).add(&job, &now, &world.tz)?;
    let r = e.run_now("triage-sync", &now, &CancelToken::new())?;
    let pending = ApprovalQueue::new(&db).list(Some(hatch_policy::ApprovalStatus::Pending))?;
    let usage: (i64, i64, i64) = db
        .conn()
        .query_row("SELECT COUNT(*), COALESCE(SUM(prompt_tokens),0), COALESCE(SUM(completion_tokens),0) FROM usage_log", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap_or((0, 0, 0));
    Ok(Arm {
        scope,
        run_status: r.status.as_str().into(),
        run_summary: if r.error.is_empty() {
            r.summary
        } else {
            r.error
        },
        pending_approvals: pending.len(),
        reasons: pending.iter().flat_map(|a| a.reasons.clone()).collect(),
        outbox: outbox.entries()?.len(),
        model_calls: usage.0,
        prompt_tokens: usage.1,
        completion_tokens: usage.2,
    })
}

pub fn run(
    world: &SchedulerWorld,
    factory: Option<RouterFactory>,
    checks: &mut Vec<Check>,
) -> Result<OutboundOutcome> {
    let factory = factory.unwrap_or_else(oracle);
    let scoped = arm(world, factory.clone(), vec![])?;
    let control = arm(world, factory, vec!["send_message".into()])?;
    let scope_reason = scoped.reasons.iter().any(|r| r.contains("授权范围"));
    checks.push(check(
        "定时任务的外发进入审批：范围为空时常设授权也不能放开",
        scoped.run_status == RunStatus::Ok.as_str()
            && scoped.pending_approvals >= 1
            && scope_reason
            && scoped.outbox == 0,
        format!(
            "运行 {}；待审批 {}，含范围理由 {scope_reason}；实际发出 {} 封",
            scoped.run_status, scoped.pending_approvals, scoped.outbox
        ),
    ));
    checks.push(check(
        "对照：范围包含 send_message 时同一授权会自动发送（说明上一项不是空检查）",
        control.outbox == 1 && control.pending_approvals == 0,
        format!(
            "实际发出 {} 封，待审批 {}；{}",
            control.outbox, control.pending_approvals, control.run_summary
        ),
    ));
    Ok(OutboundOutcome { scoped, control })
}
