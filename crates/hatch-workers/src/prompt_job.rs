//! `agent_prompt` jobs: a stored prompt run as one unattended agent turn.
//!
//! This is the generic kind the `schedule_create` tool targets by default:
//! the prompt is data, the handler knows nothing about any particular job's
//! purpose. Lives here (not in the CLI) so evaluations drive the same code
//! the daemon runs.

use crate::triage_sync::scope_classes;
use crate::{deliver, make_router, record_usage, RouterFactory, WorkerOutputs};
use hatch_agent::{Agent, AgentConfig, RunOptions, SessionStore, StopReason};
use hatch_core::Error;
use hatch_policy::{AttentionPolicy, Gatekeeper, PolicyConfig};
use hatch_scheduler::{JobContext, JobHandler, JobOutcome};
use hatch_tools::{ExecutorRegistry, Registry};
use std::sync::Arc;

/// Job kind handled by [`PromptWorker`].
pub const KIND: &str = hatch_tools::builtin::schedule::PROMPT_KIND;

/// Runs an `agent_prompt` job: one agent turn with the stored prompt,
/// in a per-job session, under a gatekeeper scoped to the job's
/// `scope` (out-of-scope external actions queue for approval, exactly
/// like the triage worker). The answer lands in worker outputs and is
/// delivered per the job's delivery setting.
pub struct PromptWorker {
    pub router: Option<RouterFactory>,
    pub policy: PolicyConfig,
    pub executors: Arc<ExecutorRegistry>,
    pub tools: Registry,
    pub attention: AttentionPolicy,
    pub user_name: String,
    pub user_email: String,
    pub max_steps: usize,
    pub max_tokens: Option<u32>,
    pub skills: Vec<String>,
    pub skill_tools: Option<Vec<String>>,
}

impl JobHandler for PromptWorker {
    fn run(&self, ctx: &JobContext) -> hatch_core::Result<JobOutcome> {
        let prompt = ctx
            .job
            .params
            .get("prompt")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                Error::Invalid(format!(
                    "agent_prompt job '{}' has no prompt in params",
                    ctx.job.name
                ))
            })?
            .to_string();
        let router = make_router(&self.router)
            .map_err(|why| Error::Config(format!("{why}，无法执行定时任务")))?;
        let gate = Gatekeeper::scoped(self.policy.clone(), scope_classes(&ctx.job.scope)?);
        let sessions = SessionStore::new(ctx.db);
        let title = format!("scheduled:{}", ctx.job.name);
        let session = match sessions
            .list(false, 200)?
            .into_iter()
            .find(|s| s.title == title)
        {
            Some(s) => s.id,
            None => sessions.create(&title, &ctx.now)?,
        };
        let agent = Agent {
            db: ctx.db,
            router: &router,
            tools: &self.tools,
            executors: &self.executors,
            gate: &gate,
            config: AgentConfig {
                user_name: self.user_name.clone(),
                user_email: self.user_email.clone(),
                tz: ctx.tz,
                max_steps: self.max_steps,
                max_tokens: self.max_tokens,
                skills: self.skills.clone(),
                skill_tools: self.skill_tools.clone(),
                origin: Some(format!("scheduled:{}", ctx.job.name)),
                system_note: Some(
                    "你正在执行一个定时任务：用户不在场，你的回答会作为任务结果保存并按任务设置送达。不要向用户提问；不确定的事直接说明。超出授权范围的对外动作会进入审批队列，不要声称已经完成。".into(),
                ),
                ..AgentConfig::default()
            },
        };
        let report = agent.run(
            &session,
            &prompt,
            &ctx.now,
            RunOptions {
                cancel: ctx.cancel.clone(),
                ..Default::default()
            },
        )?;
        let _ = record_usage(ctx.db, &router, &ctx.now);
        if let StopReason::Cancelled(why) = &report.stop {
            return Err(Error::Cancelled(why.clone()));
        }
        let content = report
            .answer
            .clone()
            .unwrap_or_else(|| report.receipt());
        let out_id = WorkerOutputs::new(ctx.db).add(
            KIND,
            &format!("定时任务 {}", ctx.job.name),
            &content,
            &ctx.now,
        )?;
        let first: String = content.chars().take(120).collect();
        let mut summary = first.replace('\n', " ");
        if !report.approvals.is_empty() {
            summary.push_str(&format!(
                "（另有 {} 项动作待你审批）",
                report.approvals.len()
            ));
        }
        deliver(ctx.db, &self.attention, ctx.job, &summary, &out_id, &ctx.now)?;
        Ok(JobOutcome::Done { summary })
    }
}
