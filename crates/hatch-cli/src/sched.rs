//! Wiring the scheduler: built-in workers with their dependencies.

use crate::app::{build_router, App, Limits};
use anyhow::Result;
use hatch_core::Error;
use hatch_policy::{Notifier, Priority};
use hatch_scheduler::{DaemonPaths, Engine, JobStore, NewJob};
use hatch_triage::TriageConfig;
use hatch_workers::triage_sync::SourceFactory;
use hatch_workers::{
    brief, memory_extract, memory_flush, memory_synthesis, triage_sync, BriefWorker,
    MemoryExtractWorker, MemoryFlushWorker, MemorySynthesisWorker, RouterFactory, TriageSyncWorker,
};
use hatch_goals::FeedWorker;
use std::sync::Arc;

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
        .register(triage_sync::KIND, Arc::new(triage))
        .register(memory_flush::KIND, Arc::new(flush))
        .register(memory_extract::KIND, Arc::new(extract))
        .register(memory_synthesis::KIND, Arc::new(synthesis))
        .register(hatch_agent::worker::KIND, Arc::new(compaction))
        .register(hatch_goals::FEED_KIND, Arc::new(feed))
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
    for j in [&mut brief, &mut triage, &mut flush, &mut extract, &mut synthesis, &mut feed] {
        j.enabled = false;
        j.owner = "system".into();
    }
    vec![brief, triage, flush, extract, synthesis, feed]
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
}
