//! Wiring the scheduler: built-in workers with their dependencies.

use crate::app::{build_router, App, Limits};
use anyhow::Result;
use hatch_agent::{Agent, AgentConfig, RunOptions, SessionStore, StopReason};
use hatch_core::Error;
use hatch_policy::{AttentionPolicy, Gatekeeper, Notifier, PolicyConfig, Priority};
use hatch_scheduler::{
    DaemonPaths, Engine, JobContext, JobHandler, JobOutcome, JobStore, NewJob,
};
use hatch_tools::builtin::{add_general_tools, default_registry};
use hatch_tools::Registry;
use hatch_triage::TriageConfig;
use hatch_workers::triage_sync::{scope_classes, SourceFactory};
use hatch_workers::{
    brief, deliver, make_router, memory_extract, memory_flush, memory_synthesis, record_usage,
    triage_sync, weekly_report, BriefWorker, MemoryExtractWorker, MemoryFlushWorker,
    MemorySynthesisWorker, RouterFactory, TriageSyncWorker, WeeklyReportWorker, WorkerOutputs,
};
use hatch_goals::FeedWorker;
use std::sync::Arc;

/// Job kind that runs a stored prompt as one agent turn. This is the
/// generic kind the `schedule_create` tool targets by default: the
/// prompt is data, the handler knows nothing about any particular
/// job's purpose.
pub const PROMPT_KIND: &str = hatch_tools::builtin::schedule::PROMPT_KIND;

/// Every job kind the engine registers, in registration order. The
/// `create_job` executor validates against this list, so an approved
/// payload can never create a job nothing can run.
pub fn job_kinds() -> Vec<String> {
    [
        brief::KIND,
        weekly_report::KIND,
        triage_sync::KIND,
        memory_flush::KIND,
        memory_extract::KIND,
        memory_synthesis::KIND,
        hatch_agent::worker::KIND,
        hatch_goals::FEED_KIND,
        PROMPT_KIND,
    ]
    .iter()
    .map(|k| k.to_string())
    .collect()
}

/// Runs an `agent_prompt` job: one agent turn with the stored prompt,
/// in a per-job session, under a gatekeeper scoped to the job's
/// `scope` (out-of-scope external actions queue for approval, exactly
/// like the triage worker). The answer lands in worker outputs and is
/// delivered per the job's delivery setting.
pub struct PromptWorker {
    router: Option<RouterFactory>,
    policy: PolicyConfig,
    executors: Arc<hatch_tools::ExecutorRegistry>,
    tools: Registry,
    attention: AttentionPolicy,
    user_name: String,
    user_email: String,
    max_steps: usize,
    max_tokens: Option<u32>,
    skills: Vec<String>,
    skill_tools: Option<Vec<String>>,
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
            PROMPT_KIND,
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

/// Builds a fresh router (fresh call budget) for every job run, or `None`
/// when no model is configured at all.
pub fn router_factory(app: &App) -> Option<RouterFactory> {
    if app.config.resolved_models().is_empty() {
        return None;
    }
    let (config, home) = (app.config.clone(), app.home.clone());
    let limits = Limits {
        max_calls: Some(app.config.scheduler.max_calls_per_job),
        max_tokens: app.config.limits.max_tokens,
        reasoning_effort: app.config.limits.effective_reasoning_effort(),
    };
    Some(Arc::new(move || {
        build_router(&config, &home, limits.clone()).map_err(|e| Error::Config(format!("{e:#}")))
    }))
}

pub fn mail_configured(app: &App) -> bool {
    let m = &app.config.mail;
    (m.source == "fixture" && !m.fixture_path.is_empty()) || !m.username.is_empty()
}

pub fn daemon_paths(app: &App) -> DaemonPaths {
    DaemonPaths::in_dir(&app.home)
}

/// The engine with every built-in job kind registered.
pub fn engine(app: &App) -> Result<Engine<'_>> {
    let router = router_factory(app);
    let attention = app.attention();
    let home = app.home.clone();
    let source: SourceFactory = Arc::new(move || {
        let app = App::open(Some(&home)).map_err(|e| Error::Config(format!("{e:#}")))?;
        app.source(None)
            .map_err(|e| Error::Connector(format!("{e:#}")))
    });
    let triage = TriageSyncWorker {
        router: router.clone(),
        policy: app.policy()?.config,
        executors: Arc::new(app.executors(false)?),
        source,
        config: TriageConfig {
            user_name: app.config.user.name.clone(),
            user_email: app.user_email(),
            tz: app.tz,
            fetch_limit: app.config.mail.fetch_limit,
            approval_ttl_hours: 72,
        },
        attention: attention.clone(),
    };
    let brief = BriefWorker {
        router: router.clone(),
        attention: attention.clone(),
        user_name: app.config.user.name.clone(),
        mail_configured: mail_configured(app),
    };
    let weekly = WeeklyReportWorker {
        router: router.clone(),
        attention: attention.clone(),
        user_name: app.config.user.name.clone(),
        mail_configured: mail_configured(app),
    };
    let flush = MemoryFlushWorker {
        router,
        attention: attention.clone(),
    };
    let extract = MemoryExtractWorker {
        router: router_factory(app),
        attention: attention.clone(),
    };
    let synthesis = MemorySynthesisWorker {
        router: router_factory(app),
    };
    let compaction = hatch_agent::worker::CompactionWorker {
        router: router_factory(app),
    };
    let feed = FeedWorker {
        router: router_factory(app),
        attention: attention.clone(),
        user_name: app.config.user.name.clone(),
        mail_configured: mail_configured(app),
        prompt: app.config.feed.prompt.clone(),
    };
    // agent_prompt: the same tool surface as a chat turn (general +
    // artifact + skill tools, skill allowlist applied), no delegation.
    let general = app.general_tools(None)?;
    let mut prompt_tools = add_general_tools(default_registry(), &general);
    for t in app.artifact_tools() {
        prompt_tools = prompt_tools.with(t);
    }
    let skill_reg = Arc::new(app.skill_registry());
    prompt_tools =
        hatch_tools::builtin::skills::add_skill_tools(prompt_tools, skill_reg, &app.home);
    let active = app.active_skills();
    let mut skill_tools: Option<Vec<String>> = None;
    if !active.is_empty() {
        let mut allowed: Vec<String> = Vec::new();
        for s in &active {
            for t in &s.tools {
                if !allowed.contains(t) {
                    allowed.push(t.clone());
                }
            }
        }
        if !allowed.is_empty() {
            skill_tools = Some(allowed.clone());
            let mut keep: Vec<&str> = allowed.iter().map(String::as_str).collect();
            keep.extend(["list_skills", "load_skill"]);
            prompt_tools = prompt_tools.filtered(&keep);
        }
    }
    let skill_bodies: Vec<String> = active
        .iter()
        .map(|s| format!("技能 {}：\n{}", s.name, s.body))
        .collect();
    let prompt_worker = PromptWorker {
        router: router_factory(app),
        policy: app.policy()?.config,
        executors: Arc::new(app.executors(false)?),
        tools: prompt_tools,
        attention: attention.clone(),
        user_name: app.config.user.name.clone(),
        user_email: app.user_email(),
        max_steps: app.config.agent.max_steps,
        max_tokens: app.config.limits.max_tokens,
        skills: skill_bodies,
        skill_tools,
    };
    let tz = app.tz;
    let notice_attention = attention;
    let home = app.home.clone();
    // 0 in config means "not set": fall back to the default instead of
    // disabling heartbeats (0 disables the thread only via the builder,
    // for tests).
    let heartbeat_secs = if app.config.tasks.heartbeat_secs == 0 {
        hatch_scheduler::DEFAULT_HEARTBEAT_SECS
    } else {
        app.config.tasks.heartbeat_secs
    };
    Ok(Engine::new(&app.db, app.tz)
        .with_grace(chrono::Duration::minutes(
            app.config.scheduler.grace_minutes.max(0),
        ))
        .with_heartbeat_secs(heartbeat_secs)
        .register(brief::KIND, Arc::new(brief))
        .register(weekly_report::KIND, Arc::new(weekly))
        .register(triage_sync::KIND, Arc::new(triage))
        .register(memory_flush::KIND, Arc::new(flush))
        .register(memory_extract::KIND, Arc::new(extract))
        .register(memory_synthesis::KIND, Arc::new(synthesis))
        .register(hatch_agent::worker::KIND, Arc::new(compaction))
        .register(hatch_goals::FEED_KIND, Arc::new(feed))
        .register(PROMPT_KIND, Arc::new(prompt_worker))
        .with_notice(Arc::new(move |job, text| {
            // Notices are rare (a job got disabled); a short-lived connection is fine.
            if let Ok(db) = hatch_store::Db::open(&home.join("hatch.db")) {
                let mut policy = notice_attention.clone();
                policy.tz = tz;
                let _ = Notifier::new(&db, policy).notify(
                    Priority::High,
                    text,
                    &format!(
                        "job-disabled:{}:{}",
                        job.id,
                        hatch_core::time::now().timestamp()
                    ),
                    &hatch_core::time::now(),
                );
            }
        })))
}

/// Default jobs created (disabled) by `hatch init`.
pub fn default_jobs() -> Vec<NewJob> {
    let mut brief = NewJob::new("daily-brief", brief::KIND, "daily 08:30");
    brief.catch_up = hatch_scheduler::CatchUp::Once;
    brief.done_condition = "当天的简报已生成（同一天只生成一次）".into();
    let mut triage = NewJob::new("triage-sync", triage_sync::KIND, "every 30m");
    triage.done_condition = "本次拉取的新邮件都已分诊；外发全部进入审批队列".into();
    let mut flush = NewJob::new("memory-flush", memory_flush::KIND, "daily 03:15");
    flush.delivery = hatch_scheduler::Delivery::Silent;
    flush.done_condition = "整理报告已生成；只合并近乎相同的记录，其余只提议".into();
    let mut extract = NewJob::new("memory-extract", memory_extract::KIND, "daily 04:00");
    extract.delivery = hatch_scheduler::Delivery::Silent;
    extract.done_condition = "新会话内容已提炼为记忆；冲突只提议不自动写".into();
    let mut synthesis = NewJob::new("memory-synthesis", memory_synthesis::KIND, "daily 02:30");
    synthesis.delivery = hatch_scheduler::Delivery::Silent;
    synthesis.done_condition = "“当前理解”文档已更新；只读摘要，不写新记忆".into();
    let mut feed = NewJob::new("daily-feed", hatch_goals::FEED_KIND, "daily 07:30");
    feed.done_condition = "当天的动态已生成（同一天只生成一次），每条都标明来源事实".into();
    let mut weekly = NewJob::new("weekly-report", weekly_report::KIND, "weekly MON 08:30");
    weekly.done_condition = "本周周报已生成（同一周只生成一次）".into();
    for j in [
        &mut brief,
        &mut triage,
        &mut flush,
        &mut extract,
        &mut synthesis,
        &mut feed,
        &mut weekly,
    ] {
        j.enabled = false;
        j.owner = "system".into();
    }
    vec![brief, triage, flush, extract, synthesis, feed, weekly]
}

/// Create the default jobs that do not exist yet. Returns the names created.
pub fn ensure_default_jobs(app: &App) -> Result<Vec<String>> {
    let store = JobStore::new(&app.db);
    let now = hatch_core::time::now();
    let mut created = Vec::new();
    for j in default_jobs() {
        if store.find(&j.name)?.is_none() {
            store.add(&j, &now, &app.tz)?;
            created.push(j.name.clone());
        }
    }
    Ok(created)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_registers_compaction_worker() {
        let dir = tempfile::tempdir().unwrap();
        let app = App::open(Some(dir.path())).unwrap();
        let engine = engine(&app).unwrap();
        assert!(engine
            .kinds()
            .contains(&hatch_agent::worker::KIND.to_string()));
    }

    #[test]
    fn engine_registers_memory_extract_and_default_job_exists() {
        let dir = tempfile::tempdir().unwrap();
        let app = App::open(Some(dir.path())).unwrap();
        let engine = engine(&app).unwrap();
        assert!(engine.kinds().contains(&memory_extract::KIND.to_string()));
        let jobs = default_jobs();
        let extract = jobs.iter().find(|j| j.name == "memory-extract").unwrap();
        assert_eq!(extract.kind, memory_extract::KIND);
        assert!(!extract.enabled, "default jobs ship disabled");
    }

    #[test]
    fn engine_registers_memory_synthesis_and_default_job_is_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let app = App::open(Some(dir.path())).unwrap();
        let engine = engine(&app).unwrap();
        assert!(engine.kinds().contains(&memory_synthesis::KIND.to_string()));
        let jobs = default_jobs();
        let synthesis = jobs.iter().find(|j| j.name == "memory-synthesis").unwrap();
        assert_eq!(synthesis.kind, memory_synthesis::KIND);
        assert_eq!(synthesis.schedule, "daily 02:30");
        assert!(!synthesis.enabled, "default jobs ship disabled");
    }

    #[test]
    fn engine_registers_agent_prompt_and_kinds_match() {
        let dir = tempfile::tempdir().unwrap();
        let app = App::open(Some(dir.path())).unwrap();
        let engine = engine(&app).unwrap();
        assert!(engine.kinds().contains(&PROMPT_KIND.to_string()));
        let mut registered = engine.kinds();
        registered.sort();
        let mut listed = job_kinds();
        listed.sort();
        assert_eq!(registered, listed);
    }

    #[test]
    fn agent_prompt_without_model_fails_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let app = App::open(Some(dir.path())).unwrap();
        let engine = engine(&app).unwrap();
        let now = hatch_core::time::now();
        let mut job = NewJob::new("p1", PROMPT_KIND, "daily 09:00");
        job.params = serde_json::json!({"prompt": "检查一下"});
        JobStore::new(&app.db).add(&job, &now, &app.tz).unwrap();
        let r = engine
            .run_now("p1", &now, &hatch_core::CancelToken::new())
            .unwrap();
        assert_eq!(r.status, hatch_scheduler::RunStatus::Failed);
        assert!(r.error.contains("没有配置模型"), "{}", r.error);
    }

    #[test]
    fn engine_registers_weekly_report_and_default_job_is_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let app = App::open(Some(dir.path())).unwrap();
        let engine = engine(&app).unwrap();
        assert!(engine.kinds().contains(&weekly_report::KIND.to_string()));
        let jobs = default_jobs();
        let weekly = jobs.iter().find(|j| j.name == "weekly-report").unwrap();
        assert_eq!(weekly.kind, weekly_report::KIND);
        assert_eq!(weekly.schedule, "weekly MON 08:30");
        assert!(!weekly.enabled, "default jobs ship disabled");
    }
}
