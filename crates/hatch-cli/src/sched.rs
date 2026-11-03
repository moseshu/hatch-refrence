//! Wiring the scheduler: built-in workers with their dependencies.

use crate::app::{build_router, App, Limits};
use anyhow::Result;
use hatch_core::Error;
use hatch_policy::{Notifier, Priority};
use hatch_scheduler::{DaemonPaths, Engine, JobStore, NewJob};
use hatch_triage::TriageConfig;
use hatch_workers::triage_sync::SourceFactory;
use hatch_workers::{
    brief, memory_flush, triage_sync, BriefWorker, MemoryFlushWorker, RouterFactory,
    TriageSyncWorker,
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
    Ok(Engine::new(&app.db, app.tz)
        .with_grace(chrono::Duration::minutes(
            app.config.scheduler.grace_minutes.max(0),
        ))
        .register(brief::KIND, Arc::new(brief))
        .register(triage_sync::KIND, Arc::new(triage))
        .register(memory_flush::KIND, Arc::new(flush))
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
    let mut feed = NewJob::new("daily-feed", hatch_goals::FEED_KIND, "daily 07:30");
    feed.done_condition = "当天的动态已生成（同一天只生成一次），每条都标明来源事实".into();
    for j in [&mut brief, &mut triage, &mut flush, &mut feed] {
        j.enabled = false;
        j.owner = "system".into();
    }
    vec![brief, triage, flush, feed]
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
