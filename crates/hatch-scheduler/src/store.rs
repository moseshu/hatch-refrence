//! Persistence for job definitions (`jobs`), execution records (`job_runs`)
//! and per-job dedup keys (`job_dedup`) — three tables, never mixed.

use crate::job::{CatchUp, Delivery, Job, JobRun, NewJob, RunStatus};
use crate::schedule::Schedule;
use chrono_tz::Tz;
use hatch_core::time::{parse_rfc3339, to_rfc3339, Timestamp};
use hatch_core::{ids, Error, Result};
use hatch_store::{params, rusqlite, Db, DbResultExt, OptionalExtension, Row};

const JOB_COLS: &str = "id, name, kind, schedule, params, scope, owner, delivery, done_condition, catch_up, \
     max_failures, timeout_secs, enabled, disabled_reason, created_at, anchor_at, next_run_at, retry_at, \
     consecutive_failures, last_run_at, last_status";

fn opt_ts(s: Option<String>) -> Option<Timestamp> {
    s.and_then(|s| parse_rfc3339(&s).ok())
}

fn conv<T>(r: Result<T>) -> rusqlite::Result<T> {
    r.map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(e.to_string()))))
}

fn job_from_row(r: &Row) -> rusqlite::Result<Job> {
    let schedule: String = r.get(3)?;
    let params: String = r.get(4)?;
    let scope: String = r.get(5)?;
    Ok(Job {
        id: r.get(0)?,
        name: r.get(1)?,
        kind: r.get(2)?,
        schedule: conv(Schedule::parse(&schedule))?,
        params: serde_json::from_str(&params).unwrap_or_default(),
        scope: serde_json::from_str(&scope).unwrap_or_default(),
        owner: r.get(6)?,
        delivery: conv(Delivery::parse(&r.get::<_, String>(7)?))?,
        done_condition: r.get(8)?,
        catch_up: conv(CatchUp::parse(&r.get::<_, String>(9)?))?,
        max_failures: r.get::<_, i64>(10)? as u32,
        timeout_secs: r.get::<_, i64>(11)? as u64,
        enabled: r.get::<_, i64>(12)? != 0,
        disabled_reason: r.get(13)?,
        created_at: conv(parse_rfc3339(&r.get::<_, String>(14)?))?,
        anchor_at: conv(parse_rfc3339(&r.get::<_, String>(15)?))?,
        next_run_at: opt_ts(r.get(16)?),
        retry_at: opt_ts(r.get(17)?),
        consecutive_failures: r.get::<_, i64>(18)? as u32,
        last_run_at: opt_ts(r.get(19)?),
        last_status: r.get(20)?,
    })
}

fn run_from_row(r: &Row) -> rusqlite::Result<JobRun> {
    Ok(JobRun {
        id: r.get(0)?,
        job_id: r.get(1)?,
        trigger: r.get(2)?,
        scheduled_for: opt_ts(r.get(3)?),
        started_at: conv(parse_rfc3339(&r.get::<_, String>(4)?))?,
        finished_at: opt_ts(r.get(5)?),
        status: RunStatus::parse(&r.get::<_, String>(6)?),
        missed: r.get::<_, i64>(7)? as u32,
        summary: r.get(8)?,
        error: r.get(9)?,
    })
}

pub struct JobStore<'a> {
    db: &'a Db,
}

impl<'a> JobStore<'a> {
    pub fn new(db: &'a Db) -> Self {
        JobStore { db }
    }

    /// Validate and store a job; its first firing is computed from `now`.
    pub fn add(&self, j: &NewJob, now: &Timestamp, tz: &Tz) -> Result<Job> {
        if j.name.trim().is_empty() || j.kind.trim().is_empty() {
            return Err(Error::Invalid("job needs a name and a kind".into()));
        }
        if self.find(&j.name)?.is_some() {
            return Err(Error::Invalid(format!("a job named {:?} already exists", j.name)));
        }
        if j.max_failures == 0 || j.timeout_secs == 0 {
            return Err(Error::Invalid("max_failures and timeout_secs must be positive".into()));
        }
        let schedule = Schedule::parse(&j.schedule)?;
        let next = schedule.next_after(now, now, tz);
        if next.is_none() {
            return Err(Error::Invalid(format!("schedule {schedule} never fires after now")));
        }
        let id = ids::random_id("job");
        self.db
            .conn()
            .execute(
                &format!("INSERT INTO jobs({JOB_COLS}) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,'',?14,?14,?15,NULL,0,NULL,'')"),
                params![
                    id,
                    j.name.trim(),
                    j.kind.trim(),
                    schedule.to_string(),
                    j.params.to_string(),
                    serde_json::to_string(&j.scope)?,
                    j.owner,
                    j.delivery.as_str(),
                    j.done_condition,
                    j.catch_up.as_str(),
                    j.max_failures as i64,
                    j.timeout_secs as i64,
                    j.enabled as i64,
                    to_rfc3339(now),
                    next.map(|t| to_rfc3339(&t)),
                ],
            )
            .db()?;
        self.db.audit("scheduler", "job_added", &format!("{} ({}) {} kind={}", j.name, id, schedule, j.kind))?;
        self.get(&id)
    }

    /// By id or by name.
    pub fn find(&self, id_or_name: &str) -> Result<Option<Job>> {
        self.db
            .conn()
            .query_row(&format!("SELECT {JOB_COLS} FROM jobs WHERE id = ?1 OR name = ?1"), params![id_or_name], job_from_row)
            .optional()
            .db()
    }

    pub fn get(&self, id_or_name: &str) -> Result<Job> {
        self.find(id_or_name)?.ok_or_else(|| Error::NotFound(format!("job {id_or_name}")))
    }

    pub fn list(&self) -> Result<Vec<Job>> {
        let c = self.db.conn();
        let mut st = c.prepare(&format!("SELECT {JOB_COLS} FROM jobs ORDER BY created_at, name")).db()?;
        let rows = st.query_map([], job_from_row).db()?;
        rows.collect::<rusqlite::Result<Vec<_>>>().db()
    }

    /// Enabled jobs whose firing (or retry) time has come.
    pub fn due(&self, now: &Timestamp) -> Result<Vec<Job>> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|j| j.enabled && j.next_run_at.is_some_and(|t| t <= *now) && !j.retry_at.is_some_and(|t| t > *now))
            .collect())
    }

    /// Enable (re-anchoring intervals at `now` and clearing failures) or disable.
    pub fn set_enabled(&self, id: &str, enabled: bool, reason: &str, now: &Timestamp, tz: &Tz) -> Result<Job> {
        let job = self.get(id)?;
        if enabled {
            let next = job.schedule.next_after(now, now, tz);
            self.db
                .conn()
                .execute(
                    "UPDATE jobs SET enabled = 1, disabled_reason = '', anchor_at = ?2, next_run_at = ?3, retry_at = NULL,
                     consecutive_failures = 0 WHERE id = ?1",
                    params![job.id, to_rfc3339(now), next.map(|t| to_rfc3339(&t))],
                )
                .db()?;
        } else {
            self.db
                .conn()
                .execute("UPDATE jobs SET enabled = 0, disabled_reason = ?2 WHERE id = ?1", params![job.id, reason])
                .db()?;
        }
        self.db.audit("scheduler", if enabled { "job_enabled" } else { "job_disabled" }, &format!("{} {reason}", job.name))?;
        self.get(&job.id)
    }

    /// Replace a job's schedule string, re-anchoring the next firing at
    /// `now`. A schedule that never fires after now is rejected.
    pub fn set_schedule(&self, id: &str, schedule: &str, now: &Timestamp, tz: &Tz) -> Result<Job> {
        let job = self.get(id)?;
        let parsed = Schedule::parse(schedule)?;
        let next = parsed.next_after(now, now, tz);
        if next.is_none() {
            return Err(Error::Invalid(format!(
                "schedule {parsed} 之后没有下一次触发时间"
            )));
        }
        self.db
            .conn()
            .execute(
                "UPDATE jobs SET schedule = ?2, anchor_at = ?3, next_run_at = ?4, retry_at = NULL WHERE id = ?1",
                params![job.id, parsed.to_string(), to_rfc3339(now), next.map(|t| to_rfc3339(&t))],
            )
            .db()?;
        self.db.audit(
            "scheduler",
            "job_rescheduled",
            &format!("{} {parsed}", job.name),
        )?;
        self.get(&job.id)
    }

    /// Remove a job and its dedup keys; run history is kept for the record.
    pub fn remove(&self, id: &str) -> Result<()> {
        let job = self.get(id)?;
        let c = self.db.conn();
        c.execute("DELETE FROM jobs WHERE id = ?1", params![job.id]).db()?;
        c.execute("DELETE FROM job_dedup WHERE job_id = ?1", params![job.id]).db()?;
        drop(c);
        self.db.audit("scheduler", "job_removed", &job.name)?;
        Ok(())
    }

    /// Scheduling state after a firing was handled.
    pub fn set_state(
        &self,
        id: &str,
        next_run_at: Option<Timestamp>,
        retry_at: Option<Timestamp>,
        failures: u32,
        last_run_at: Option<Timestamp>,
        last_status: &str,
    ) -> Result<()> {
        self.db
            .conn()
            .execute(
                "UPDATE jobs SET next_run_at = ?2, retry_at = ?3, consecutive_failures = ?4,
                 last_run_at = COALESCE(?5, last_run_at), last_status = ?6 WHERE id = ?1",
                params![
                    id,
                    next_run_at.map(|t| to_rfc3339(&t)),
                    retry_at.map(|t| to_rfc3339(&t)),
                    failures as i64,
                    last_run_at.map(|t| to_rfc3339(&t)),
                    last_status
                ],
            )
            .db()?;
        Ok(())
    }

    pub fn start_run(&self, job_id: &str, trigger: &str, scheduled_for: Option<Timestamp>, now: &Timestamp) -> Result<String> {
        let id = ids::random_id("jrun");
        self.db
            .conn()
            .execute(
                "INSERT INTO job_runs(id, job_id, trigger, scheduled_for, started_at, status) VALUES(?1,?2,?3,?4,?5,'running')",
                params![id, job_id, trigger, scheduled_for.map(|t| to_rfc3339(&t)), to_rfc3339(now)],
            )
            .db()?;
        Ok(id)
    }

    pub fn finish_run(&self, run_id: &str, status: RunStatus, summary: &str, error: &str, now: &Timestamp) -> Result<()> {
        self.db
            .conn()
            .execute(
                "UPDATE job_runs SET status = ?2, summary = ?3, error = ?4, finished_at = ?5 WHERE id = ?1",
                params![run_id, status.as_str(), hatch_core::secrets::redact(summary), hatch_core::secrets::redact(error), to_rfc3339(now)],
            )
            .db()?;
        Ok(())
    }

    /// Record `count` firings that never ran.
    pub fn record_missed(&self, job_id: &str, count: u32, first: &Timestamp, last: &Timestamp, now: &Timestamp) -> Result<()> {
        let id = ids::random_id("jrun");
        let summary = format!("错过 {count} 次触发（{} — {}），daemon 未运行或任务被阻塞", to_rfc3339(first), to_rfc3339(last));
        self.db
            .conn()
            .execute(
                "INSERT INTO job_runs(id, job_id, trigger, scheduled_for, started_at, finished_at, status, missed, summary)
                 VALUES(?1,?2,'missed',?3,?4,?4,'missed',?5,?6)",
                params![id, job_id, to_rfc3339(first), to_rfc3339(now), count as i64, summary],
            )
            .db()?;
        Ok(())
    }

    /// Newest first.
    pub fn history(&self, job_id: Option<&str>, limit: usize) -> Result<Vec<JobRun>> {
        let c = self.db.conn();
        let sql = "SELECT id, job_id, trigger, scheduled_for, started_at, finished_at, status, missed, summary, error
                   FROM job_runs WHERE (?1 IS NULL OR job_id = ?1) ORDER BY started_at DESC, rowid DESC LIMIT ?2";
        let mut st = c.prepare(sql).db()?;
        let rows = st.query_map(params![job_id, limit as i64], run_from_row).db()?;
        rows.collect::<rusqlite::Result<Vec<_>>>().db()
    }

    /// Runs left in `running` by a crash become `failed` (they are retried).
    pub fn reap_interrupted(&self, now: &Timestamp) -> Result<usize> {
        let n = self
            .db
            .conn()
            .execute(
                "UPDATE job_runs SET status = 'failed', error = '进程在运行中退出（崩溃或被强制结束）', finished_at = ?1
                 WHERE status = 'running'",
                params![to_rfc3339(now)],
            )
            .db()?;
        Ok(n)
    }

    pub fn dedup_seen(&self, job_id: &str, key: &str) -> Result<bool> {
        Ok(self
            .db
            .conn()
            .query_row("SELECT 1 FROM job_dedup WHERE job_id = ?1 AND key = ?2", params![job_id, key], |_| Ok(()))
            .optional()
            .db()?
            .is_some())
    }

    /// Returns `false` if the key was already present.
    pub fn dedup_mark(&self, job_id: &str, key: &str, now: &Timestamp) -> Result<bool> {
        let n = self
            .db
            .conn()
            .execute(
                "INSERT OR IGNORE INTO job_dedup(job_id, key, created_at) VALUES(?1,?2,?3)",
                params![job_id, key, to_rfc3339(now)],
            )
            .db()?;
        Ok(n == 1)
    }
}
