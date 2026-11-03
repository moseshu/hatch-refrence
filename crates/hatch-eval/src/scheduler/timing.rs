//! Timing scenarios on a fake clock: DST, downtime, failures, timeouts and
//! graceful stop. No model is involved.

use super::{ts, DstSpec, SchedulerWorld};
use crate::report::Check;
use chrono::Duration;
use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::{CancelToken, Error, Result};
use hatch_scheduler::{
    CatchUp, Daemon, DaemonConfig, DaemonPaths, Engine, FakeClock, FnHandler, JobContext, JobHandler, JobOutcome, JobRun,
    JobStore, NewJob, RunStatus, Trigger,
};
use hatch_store::Db;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

fn check(name: &str, passed: bool, detail: impl Into<String>) -> Check {
    Check { name: name.into(), passed, detail: detail.into() }
}

fn ok_handler() -> Arc<dyn JobHandler> {
    Arc::new(FnHandler(|_: &JobContext| Ok(JobOutcome::Done { summary: "ok".into() })))
}

/// Tick every `step` from `from` to `to` (inclusive), skipping the open interval `down`.
fn drive(e: &Engine, from: Timestamp, to: Timestamp, step: Duration, down: Option<(Timestamp, Timestamp)>) -> Result<()> {
    let c = CancelToken::new();
    let mut t = from;
    while t <= to {
        if !down.is_some_and(|(a, b)| t > a && t < b) {
            e.tick(&t, &c)?;
        }
        t += step;
    }
    Ok(())
}

fn runs(db: &Db, job: &str) -> Result<Vec<JobRun>> {
    let store = JobStore::new(db);
    let id = store.get(job)?.id;
    let mut r = store.history(Some(&id), 10_000)?;
    r.reverse();
    Ok(r)
}

fn dst(world: &SchedulerWorld, spec: &DstSpec, name: &str, checks: &mut Vec<Check>, notes: &mut Vec<String>) -> Result<()> {
    let db = Db::open_in_memory()?;
    let e = Engine::new(&db, world.tz).register("k", ok_handler());
    let from = ts(&spec.from)?;
    JobStore::new(&db).add(&NewJob::new("dst", "k", &spec.schedule), &from, &world.tz)?;
    drive(&e, from, ts(&spec.to)?, Duration::minutes(world.spec.tick_minutes), None)?;
    let got: Vec<String> = runs(&db, "dst")?
        .iter()
        .filter(|r| r.status == RunStatus::Ok)
        .filter_map(|r| r.scheduled_for.map(|t| to_rfc3339(&t)))
        .collect();
    let want: Vec<String> = spec.expected_utc.iter().map(|s| ts(s).map(|t| to_rfc3339(&t))).collect::<Result<_>>()?;
    notes.push(format!("{name}（{}，{}）：实际触发 {}", spec.schedule, world.spec.timezone, got.join("、")));
    checks.push(check(name, got == want, format!("期望 {} 次：{}；实际 {} 次", want.len(), want.join("、"), got.len())));
    Ok(())
}

fn downtime(world: &SchedulerWorld, catch_up: CatchUp, checks: &mut Vec<Check>) -> Result<()> {
    let d = &world.spec.downtime;
    let db = Db::open_in_memory()?;
    let e = Engine::new(&db, world.tz).register("k", ok_handler());
    let created = ts(&d.created)?;
    let mut j = NewJob::new("sync", "k", &d.schedule);
    j.catch_up = catch_up;
    JobStore::new(&db).add(&j, &created, &world.tz)?;
    let (down_from, down_to) = (ts(&d.down_from)?, ts(&d.down_to)?);
    drive(&e, created, ts(&d.resume_until)?, Duration::minutes(world.spec.tick_minutes), Some((down_from, down_to)))?;
    let all = runs(&db, "sync")?;
    let missed: u32 = all.iter().filter(|r| r.status == RunStatus::Missed).map(|r| r.missed).sum();
    let ok = |f: &dyn Fn(&JobRun) -> bool| all.iter().filter(|r| r.status == RunStatus::Ok && f(r)).count();
    let before = ok(&|r| r.started_at <= down_from);
    let caught = ok(&|r| r.trigger == Trigger::CatchUp.as_str());
    let after = ok(&|r| r.started_at > down_to && r.trigger == Trigger::Scheduled.as_str());
    let want_catch = usize::from(catch_up == CatchUp::Once);
    let passed = missed == d.expected_missed && caught == want_catch && before == d.runs_before && after == d.runs_after;
    checks.push(check(
        &format!("停机 {}–{}：catch_up = {}", &d.down_from[11..16], &d.down_to[11..16], catch_up.as_str()),
        passed,
        format!(
            "错过 {missed} 次（期望 {}），补跑 {caught} 次（期望 {want_catch}），停机前 {before} 次、恢复后按计划 {after} 次（期望 {}/{}）",
            d.expected_missed, d.runs_before, d.runs_after
        ),
    ));
    Ok(())
}

fn failures(world: &SchedulerWorld, checks: &mut Vec<Check>) -> Result<()> {
    let f = &world.spec.failure;
    let db = Db::open_in_memory()?;
    let notices = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = notices.clone();
    let err = f.error.clone();
    let calls = Arc::new(AtomicU32::new(0));
    let c2 = calls.clone();
    let e = Engine::new(&db, world.tz)
        .register("bad", Arc::new(FnHandler(move |_: &JobContext| Err(Error::Connector(err.clone())))))
        .register(
            "flaky",
            Arc::new(FnHandler(move |_: &JobContext| {
                if c2.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(Error::Connector("临时网络错误".into()))
                } else {
                    Ok(JobOutcome::Done { summary: "ok".into() })
                }
            })),
        )
        .with_notice(Arc::new(move |_, text| sink.lock().unwrap_or_else(|p| p.into_inner()).push(text.to_string())));
    let t0 = ts("2026-11-03T13:00:00Z")?;
    let mut j = NewJob::new("bad", "bad", "every 30m");
    j.max_failures = f.max_failures;
    JobStore::new(&db).add(&j, &t0, &world.tz)?;
    JobStore::new(&db).add(&NewJob::new("flaky", "flaky", "every 30m"), &t0, &world.tz)?;
    drive(&e, t0, t0 + Duration::hours(3), Duration::minutes(1), None)?;
    let bad = JobStore::new(&db).get("bad")?;
    let failed = runs(&db, "bad")?.iter().filter(|r| r.status == RunStatus::Failed).count();
    let notice = notices.lock().unwrap_or_else(|p| p.into_inner()).first().cloned().unwrap_or_default();
    checks.push(check(
        "连续失败后自动禁用并告知原因",
        !bad.enabled && failed == f.max_failures as usize && bad.disabled_reason.contains(&f.error) && notice.contains("hatch jobs enable"),
        format!("失败 {failed} 次后 enabled={}；原因：{}；通知：{}", bad.enabled, bad.disabled_reason, notice),
    ));
    let flaky = JobStore::new(&db).get("flaky")?;
    let fr = runs(&db, "flaky")?;
    let retried_ok = fr.iter().any(|r| r.trigger == Trigger::Retry.as_str() && r.status == RunStatus::Ok);
    checks.push(check(
        "失败后退避重试，成功即清零",
        retried_ok && flaky.enabled && flaky.consecutive_failures == 0,
        format!("运行 {} 次，重试成功 {retried_ok}，当前失败计数 {}", fr.len(), flaky.consecutive_failures),
    ));
    Ok(())
}

fn hung(ctx: &JobContext) -> Result<JobOutcome> {
    while !ctx.cancel.sleep(std::time::Duration::from_millis(50)) {}
    ctx.cancel.check()?;
    Ok(JobOutcome::Done { summary: "unreachable".into() })
}

fn timeout(world: &SchedulerWorld, checks: &mut Vec<Check>) -> Result<()> {
    let db = Db::open_in_memory()?;
    let e = Engine::new(&db, world.tz).register("hang", Arc::new(FnHandler(hung)));
    let t0 = ts("2026-11-03T13:00:00Z")?;
    let mut j = NewJob::new("hang", "hang", "every 30m");
    j.timeout_secs = 1;
    JobStore::new(&db).add(&j, &t0, &world.tz)?;
    let started = std::time::Instant::now();
    let r = e.tick(&(t0 + Duration::minutes(30)), &CancelToken::new())?;
    let elapsed = started.elapsed();
    let run = r.runs.first();
    checks.push(check(
        "每次运行必须结束：卡死的任务被看门狗停止",
        run.is_some_and(|r| r.status == RunStatus::Failed && r.error.contains("1 秒")) && elapsed < std::time::Duration::from_secs(5),
        format!("{:?} 用时 {:.1}s：{}", run.map(|r| r.status), elapsed.as_secs_f64(), run.map(|r| r.error.as_str()).unwrap_or("")),
    ));
    Ok(())
}

fn graceful_stop(world: &SchedulerWorld, checks: &mut Vec<Check>) -> Result<()> {
    let db = Db::open_in_memory()?;
    let dir = tempfile::tempdir()?;
    let n = Arc::new(AtomicU32::new(0));
    let n2 = n.clone();
    let e = Engine::new(&db, world.tz).register(
        "slow",
        Arc::new(FnHandler(move |ctx: &JobContext| {
            if n2.fetch_add(1, Ordering::SeqCst) == 0 {
                return hung(ctx);
            }
            Ok(JobOutcome::Done { summary: "done".into() })
        })),
    );
    let t0 = ts("2026-11-03T13:00:00Z")?;
    JobStore::new(&db).add(&NewJob::new("slow", "slow", "every 30m"), &t0, &world.tz)?;
    let clock = FakeClock::new(t0 + Duration::minutes(30));
    let paths = DaemonPaths::in_dir(dir.path());
    let daemon = Daemon { engine: &e, db: &db, paths: paths.clone(), config: DaemonConfig { tick: std::time::Duration::from_secs(1), ..Default::default() }, clock: &clock };
    let stop = paths.stop.clone();
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        let _ = std::fs::write(stop, "stop");
    });
    let exit = daemon.run(&CancelToken::new())?;
    let _ = stopper.join();
    let cancelled = runs(&db, "slow")?.iter().filter(|r| r.status == RunStatus::Cancelled).count();
    let state = db.meta("daemon_state")?.unwrap_or_default();
    let lock_gone = !paths.lock.exists();
    let rerun = e.tick(&(t0 + Duration::hours(2)), &CancelToken::new())?;
    let rerun_ok = rerun.runs.first().is_some_and(|r| r.trigger == Trigger::Retry && r.status == RunStatus::Ok);
    checks.push(check(
        "停止信号：任务在取消点停下、记为已取消、锁释放，下次启动重跑",
        exit.reason == "stop file" && cancelled == 1 && state == "stopped" && lock_gone && rerun_ok,
        format!("退出原因 {}；已取消 {cancelled}；状态 {state}；锁已释放 {lock_gone}；重启后重跑成功 {rerun_ok}", exit.reason),
    ));
    Ok(())
}

pub fn run(world: &SchedulerWorld, checks: &mut Vec<Check>, notes: &mut Vec<String>) -> Result<()> {
    dst(world, &world.spec.dst_fall, "DST 秋季回拨：重复的时刻只触发一次", checks, notes)?;
    dst(world, &world.spec.dst_spring, "DST 春季跳时：不存在的时刻顺延", checks, notes)?;
    downtime(world, CatchUp::None, checks)?;
    downtime(world, CatchUp::Once, checks)?;
    failures(world, checks)?;
    timeout(world, checks)?;
    graceful_stop(world, checks)?;
    Ok(())
}
