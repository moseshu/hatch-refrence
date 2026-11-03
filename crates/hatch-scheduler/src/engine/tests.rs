use super::*;
use crate::job::NewJob;
use hatch_core::time::parse_rfc3339;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

fn ts(s: &str) -> Timestamp {
    parse_rfc3339(s).unwrap()
}

const SH: Tz = chrono_tz::Asia::Shanghai;

fn ok_handler(counter: Arc<AtomicU32>) -> Arc<dyn JobHandler> {
    Arc::new(FnHandler(move |_: &JobContext| {
        counter.fetch_add(1, Ordering::SeqCst);
        Ok(JobOutcome::Done { summary: "ok".into() })
    }))
}

fn add(db: &Db, name: &str, kind: &str, schedule: &str, now: &str, f: impl FnOnce(&mut NewJob)) -> Job {
    let mut j = NewJob::new(name, kind, schedule);
    f(&mut j);
    JobStore::new(db).add(&j, &ts(now), &SH).unwrap()
}

#[test]
fn on_time_run_advances_to_the_next_firing() {
    let db = Db::open_in_memory().unwrap();
    let n = Arc::new(AtomicU32::new(0));
    let e = Engine::new(&db, SH).register("k", ok_handler(n.clone()));
    let job = add(&db, "brief", "k", "daily 08:30", "2026-11-03T00:00:00Z", |_| {});
    assert_eq!(job.next_run_at, Some(ts("2026-11-03T00:30:00Z")));
    let c = CancelToken::new();
    assert!(e.tick(&ts("2026-11-03T00:29:00Z"), &c).unwrap().runs.is_empty());
    let r = e.tick(&ts("2026-11-03T00:31:00Z"), &c).unwrap();
    assert_eq!((r.runs.len(), r.missed.len()), (1, 0));
    assert_eq!(r.runs[0].trigger, Trigger::Scheduled);
    let job = JobStore::new(&db).get("brief").unwrap();
    assert_eq!(job.next_run_at, Some(ts("2026-11-04T00:30:00Z")));
    assert_eq!(job.last_status, "ok");
    assert_eq!(n.load(Ordering::SeqCst), 1);
}

#[test]
fn downtime_records_misses_and_catches_up_at_most_once() {
    for (catch_up, runs) in [(CatchUp::None, 0), (CatchUp::Once, 1)] {
        let db = Db::open_in_memory().unwrap();
        let n = Arc::new(AtomicU32::new(0));
        let e = Engine::new(&db, SH).register("k", ok_handler(n.clone()));
        add(&db, "sync", "k", "every 30m", "2026-11-03T00:00:00Z", |j| j.catch_up = catch_up);
        // Daemon down from 00:00 to 05:15: firings 00:30 … 05:00 = 10, the last 15 min late.
        let r = e.tick(&ts("2026-11-03T05:15:00Z"), &CancelToken::new()).unwrap();
        assert_eq!(r.missed, vec![("sync".to_string(), 10)], "{catch_up:?}");
        assert_eq!(n.load(Ordering::SeqCst), runs, "{catch_up:?}");
        if runs == 1 {
            assert_eq!(r.runs[0].trigger, Trigger::CatchUp);
        }
        let job = JobStore::new(&db).get("sync").unwrap();
        assert_eq!(job.next_run_at, Some(ts("2026-11-03T05:30:00Z")));
        let missed_rows: Vec<_> = JobStore::new(&db).history(Some(&job.id), 50).unwrap().into_iter().filter(|r| r.status == RunStatus::Missed).collect();
        assert_eq!(missed_rows.len(), 1);
        assert_eq!(missed_rows[0].missed, 10);
    }
}

#[test]
fn a_late_but_within_grace_firing_still_runs_on_time() {
    let db = Db::open_in_memory().unwrap();
    let n = Arc::new(AtomicU32::new(0));
    let e = Engine::new(&db, SH).register("k", ok_handler(n.clone()));
    add(&db, "sync", "k", "every 30m", "2026-11-03T00:00:00Z", |_| {});
    // 00:30 and 01:00 are due; 01:00 is 5 minutes late (< 10 min grace).
    let r = e.tick(&ts("2026-11-03T01:05:00Z"), &CancelToken::new()).unwrap();
    assert_eq!(r.missed, vec![("sync".to_string(), 1)]);
    assert_eq!(r.runs.len(), 1);
    assert_eq!(r.runs[0].trigger, Trigger::Scheduled);
}

#[test]
fn failures_retry_with_backoff_then_auto_disable_with_a_notice() {
    let db = Db::open_in_memory().unwrap();
    let notices = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = notices.clone();
    let e = Engine::new(&db, SH)
        .register("bad", Arc::new(FnHandler(|_: &JobContext| Err(Error::Connector("IMAP 连接失败".into())))))
        .with_notice(Arc::new(move |_, text| sink.lock().unwrap().push(text.to_string())));
    add(&db, "triage", "bad", "every 30m", "2026-11-03T00:00:00Z", |j| j.max_failures = 3);
    let c = CancelToken::new();
    let r1 = e.tick(&ts("2026-11-03T00:31:00Z"), &c).unwrap();
    assert_eq!(r1.runs[0].status, RunStatus::Failed);
    let job = JobStore::new(&db).get("triage").unwrap();
    assert_eq!(job.consecutive_failures, 1);
    assert_eq!(job.next_run_at, Some(ts("2026-11-03T00:30:00Z")), "a failed firing stays pending");
    assert_eq!(job.retry_at, Some(ts("2026-11-03T00:32:00Z")));
    // Not yet time to retry.
    assert!(e.tick(&ts("2026-11-03T00:31:30Z"), &c).unwrap().runs.is_empty());
    let r2 = e.tick(&ts("2026-11-03T00:32:00Z"), &c).unwrap();
    assert_eq!(r2.runs[0].trigger, Trigger::Retry);
    let r3 = e.tick(&ts("2026-11-03T00:40:00Z"), &c).unwrap();
    assert_eq!(r3.disabled.len(), 1);
    let job = JobStore::new(&db).get("triage").unwrap();
    assert!(!job.enabled);
    assert!(job.disabled_reason.contains("连续失败 3 次"), "{}", job.disabled_reason);
    assert!(notices.lock().unwrap()[0].contains("hatch jobs enable triage"));
    assert!(e.tick(&ts("2026-11-03T05:00:00Z"), &c).unwrap().runs.is_empty(), "disabled jobs never run");
    // Re-enabling clears the failure state.
    let job = JobStore::new(&db).set_enabled("triage", true, "", &ts("2026-11-03T05:00:00Z"), &SH).unwrap();
    assert_eq!((job.consecutive_failures, job.retry_at), (0, None));
}

#[test]
fn a_success_after_failures_resets_the_counter() {
    let db = Db::open_in_memory().unwrap();
    let calls = Arc::new(AtomicU32::new(0));
    let c2 = calls.clone();
    let e = Engine::new(&db, SH).register(
        "flaky",
        Arc::new(FnHandler(move |_: &JobContext| {
            if c2.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(Error::Connector("timeout".into()))
            } else {
                Ok(JobOutcome::Done { summary: "ok".into() })
            }
        })),
    );
    add(&db, "f", "flaky", "daily 08:30", "2026-11-03T00:00:00Z", |_| {});
    let c = CancelToken::new();
    e.tick(&ts("2026-11-03T00:31:00Z"), &c).unwrap();
    let r = e.tick(&ts("2026-11-03T00:40:00Z"), &c).unwrap();
    assert_eq!(r.runs[0].status, RunStatus::Ok);
    let job = JobStore::new(&db).get("f").unwrap();
    assert_eq!((job.consecutive_failures, job.retry_at), (0, None));
    assert_eq!(job.next_run_at, Some(ts("2026-11-04T00:30:00Z")));
}

fn wait_for_cancel(ctx: &JobContext) -> Result<JobOutcome> {
    while !ctx.cancel.sleep(std::time::Duration::from_millis(50)) {}
    ctx.cancel.check()?;
    Ok(JobOutcome::Done { summary: "unreachable".into() })
}

#[test]
fn every_run_ends_the_watchdog_stops_a_hung_handler() {
    let db = Db::open_in_memory().unwrap();
    let e = Engine::new(&db, SH).register("hang", Arc::new(FnHandler(wait_for_cancel)));
    add(&db, "h", "hang", "daily 08:30", "2026-11-03T00:00:00Z", |j| j.timeout_secs = 1);
    let started = std::time::Instant::now();
    let r = e.tick(&ts("2026-11-03T00:31:00Z"), &CancelToken::new()).unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(r.runs[0].status, RunStatus::Failed);
    assert!(r.runs[0].error.contains("1 秒"), "{}", r.runs[0].error);
}

#[test]
fn cancellation_is_not_failure_and_the_firing_is_rerun() {
    let db = Db::open_in_memory().unwrap();
    let n = Arc::new(AtomicU32::new(0));
    let n2 = n.clone();
    let e = Engine::new(&db, SH).register(
        "slow",
        Arc::new(FnHandler(move |ctx: &JobContext| {
            if n2.fetch_add(1, Ordering::SeqCst) == 0 {
                return wait_for_cancel(ctx);
            }
            Ok(JobOutcome::Done { summary: "done".into() })
        })),
    );
    add(&db, "s", "slow", "daily 08:30", "2026-11-03T00:00:00Z", |_| {});
    let stop = CancelToken::new();
    let s2 = stop.clone();
    let t = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(150));
        s2.cancel("daemon stopping");
    });
    let r = e.tick(&ts("2026-11-03T00:31:00Z"), &stop).unwrap();
    t.join().unwrap();
    assert_eq!(r.runs[0].status, RunStatus::Cancelled);
    let job = JobStore::new(&db).get("s").unwrap();
    assert_eq!((job.consecutive_failures, job.next_run_at), (0, Some(ts("2026-11-03T00:30:00Z"))));
    // Next start, an hour later: the interrupted firing is re-run (not counted as missed).
    let r = e.tick(&ts("2026-11-03T01:31:00Z"), &CancelToken::new()).unwrap();
    assert_eq!((r.runs[0].trigger, r.runs[0].status), (Trigger::Retry, RunStatus::Ok));
    assert!(r.missed.is_empty());
}

#[test]
fn dedup_keys_and_once_jobs() {
    let db = Db::open_in_memory().unwrap();
    let e = Engine::new(&db, SH).register(
        "brief",
        Arc::new(FnHandler(|ctx: &JobContext| {
            let key = format!("brief:{}", ctx.now.with_timezone(&ctx.tz).date_naive());
            if !ctx.dedup_mark(&key)? {
                return Ok(JobOutcome::Skipped { reason: "今天已生成".into() });
            }
            Ok(JobOutcome::Done { summary: "生成".into() })
        })),
    );
    add(&db, "b", "brief", "once 2026-11-03T01:00:00Z", "2026-11-03T00:00:00Z", |_| {});
    let c = CancelToken::new();
    let r = e.tick(&ts("2026-11-03T01:00:00Z"), &c).unwrap();
    assert_eq!(r.runs[0].status, RunStatus::Ok);
    let job = JobStore::new(&db).get("b").unwrap();
    assert_eq!(job.next_run_at, None, "once jobs complete");
    assert!(e.tick(&ts("2026-11-04T01:00:00Z"), &c).unwrap().runs.is_empty());
    // A manual re-run on the same day is deduplicated.
    let r = e.run_now("b", &ts("2026-11-03T02:00:00Z"), &c).unwrap();
    assert_eq!(r.status, RunStatus::Skipped);
    // Unknown kinds fail loudly.
    add(&db, "x", "nope", "daily 09:00", "2026-11-03T00:00:00Z", |_| {});
    assert_eq!(e.run_now("x", &ts("2026-11-03T02:00:00Z"), &c).unwrap().status, RunStatus::Failed);
}
