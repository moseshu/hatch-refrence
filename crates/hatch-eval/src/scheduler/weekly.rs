//! Weekly-report scenario: one report per ISO week, facts gathered by
//! code, out-of-window items excluded, a repeat run in the same week
//! skipped, and the run announced on the private notification channel.
//! Pipeline mode costs no model call (the report is complete without
//! a summary); live mode adds one summary call.

use super::brief::seed_world;
use super::{ts, SchedulerWorld};
use crate::report::Check;
use chrono::Duration;
use hatch_core::time::to_rfc3339;
use hatch_core::{CancelToken, Result};
use hatch_policy::{AttentionPolicy, Notifier};
use hatch_scheduler::{Engine, JobStore, NewJob};
use hatch_store::{params, Db};
use hatch_workers::weekly_report::{self, WeeklyReportWorker};
use hatch_workers::{RouterFactory, WorkerOutputs};
use std::sync::Arc;

fn check(name: &str, passed: bool, detail: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        passed,
        detail: detail.into(),
    }
}

fn seed_task(db: &Db, id: &str, title: &str, status: &str, finished: Option<String>) {
    db.conn()
        .execute(
            "INSERT INTO tasks(id, kind, title, owner, status, last_heartbeat, created_at, updated_at, finished_at)
             VALUES(?1, 'meeting-brief', ?2, 'test', ?3, '2026-11-03T13:00:00Z', '2026-11-03T13:00:00Z', '2026-11-03T13:00:00Z', ?4)",
            params![id, title, status, finished],
        )
        .unwrap();
}

fn reports(db: &Db) -> Vec<hatch_workers::WorkerOutput> {
    WorkerOutputs::new(db)
        .recent(10)
        .unwrap_or_default()
        .into_iter()
        .filter(|o| o.kind == "weekly-report")
        .collect()
}

pub fn run(
    world: &SchedulerWorld,
    factory: Option<RouterFactory>,
    live: bool,
    checks: &mut Vec<Check>,
) -> Result<()> {
    let b = &world.spec.brief;
    let now = ts(&b.now)?;
    let db = Db::open_in_memory()?;
    seed_world(&db, world, &now)?;
    seed_task(
        &db,
        "tsk-week",
        "本周已完成：架构评审纪要",
        "done",
        Some(to_rfc3339(&(now - Duration::days(1)))),
    );
    seed_task(
        &db,
        "tsk-old",
        "十天前完成的任务",
        "done",
        Some(to_rfc3339(&(now - Duration::days(10)))),
    );
    let worker = WeeklyReportWorker {
        router: factory,
        attention: AttentionPolicy {
            tz: world.tz,
            ..Default::default()
        },
        user_name: world.spec.persona.name.clone(),
        mail_configured: true,
    };
    let e = Engine::new(&db, world.tz).register(weekly_report::KIND, Arc::new(worker));
    let local = now.with_timezone(&world.tz);
    JobStore::new(&db).add(
        &NewJob::new(
            "weekly-report",
            weekly_report::KIND,
            &format!("daily {}", local.format("%H:%M")),
        ),
        &(now - Duration::minutes(1)),
        &world.tz,
    )?;
    let c = CancelToken::new();
    e.tick(&now, &c)?;
    e.run_now("weekly-report", &(now + Duration::hours(2)), &c)?;
    e.run_now("weekly-report", &(now + Duration::hours(3)), &c)?;
    let first = reports(&db);
    checks.push(check(
        "周报：同一周只生成一份",
        first.len() == 1,
        format!("定时 1 次 + 手动 2 次后共 {} 份", first.len()),
    ));
    let content = first.first().map(|o| o.content.clone()).unwrap_or_default();
    let in_window = [
        "Q4 预算请今天确认",
        "周三评审时间",
        "发票地址变更，请回复确认",
        "本周架构精选",
        "架构评审",
        "供应商电话",
        "团队午餐",
        "季度规划会",
        "本周已完成：架构评审纪要",
    ];
    let missing: Vec<&&str> = in_window
        .iter()
        .filter(|s| !content.contains(**s))
        .collect();
    let leaked = ["十天前完成的任务"]
        .iter()
        .filter(|s| content.contains(**s))
        .collect::<Vec<_>>();
    checks.push(check(
        "周报完整：本周的邮件、日程与完成任务都在，过期内容不在",
        missing.is_empty() && leaked.is_empty(),
        format!("缺失 {missing:?}；过期残留 {leaked:?}"),
    ));
    checks.push(check(
        "周报标题是本周的 ISO 周",
        first.first().map(|o| o.title.as_str()) == Some("周报 2026-W45"),
        format!("标题 {:?}", first.first().map(|o| o.title.clone())),
    ));
    let notified = Notifier::new(
        &db,
        AttentionPolicy {
            tz: world.tz,
            ..Default::default()
        },
    )
    .pending(false)
    .unwrap_or_default()
    .iter()
    .any(|n| n.text.contains("周报已生成"));
    checks.push(check(
        "周报生成后在私有通知里有记录",
        notified,
        if notified {
            "通知已发".to_string()
        } else {
            "没有找到周报通知".to_string()
        },
    ));
    if !live {
        let calls: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM usage_log", [], |r| r.get(0))
            .unwrap_or(0);
        checks.push(check(
            "周报：无模型时零模型调用",
            calls == 0,
            format!("usage_log {calls} 条"),
        ));
    }
    e.tick(&(now + Duration::days(7)), &c)?;
    let second = reports(&db);
    checks.push(check(
        "周报：下一周生成新的一份",
        second.len() == 2,
        format!("跨周后共 {} 份", second.len()),
    ));
    Ok(())
}
