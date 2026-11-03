//! Job definitions and run records.

use crate::schedule::Schedule;
use hatch_core::time::Timestamp;
use hatch_core::{Error, Result};
use serde::Serialize;
use serde_json::Value;

/// How a job's result reaches the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// A notification routed through the attention policy.
    Notify,
    /// Included in the digest (`hatch digest`), never interrupts.
    Digest,
    /// Recorded only.
    Silent,
}

impl Delivery {
    pub fn as_str(&self) -> &'static str {
        match self {
            Delivery::Notify => "notify",
            Delivery::Digest => "digest",
            Delivery::Silent => "silent",
        }
    }

    pub fn parse(s: &str) -> Result<Delivery> {
        match s {
            "notify" => Ok(Delivery::Notify),
            "digest" => Ok(Delivery::Digest),
            "silent" => Ok(Delivery::Silent),
            _ => Err(Error::Invalid(format!("delivery must be notify, digest or silent, got {s:?}"))),
        }
    }
}

/// What to do about firings missed while the daemon was down.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatchUp {
    /// Record the misses; wait for the next regular firing.
    None,
    /// Record the misses and run once now — never once per miss.
    Once,
}

impl CatchUp {
    pub fn as_str(&self) -> &'static str {
        match self {
            CatchUp::None => "none",
            CatchUp::Once => "once",
        }
    }

    pub fn parse(s: &str) -> Result<CatchUp> {
        match s {
            "none" => Ok(CatchUp::None),
            "once" => Ok(CatchUp::Once),
            _ => Err(Error::Invalid(format!("catch_up must be none or once, got {s:?}"))),
        }
    }
}

/// Why a run started.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    Scheduled,
    CatchUp,
    Retry,
    Manual,
}

impl Trigger {
    pub fn as_str(&self) -> &'static str {
        match self {
            Trigger::Scheduled => "scheduled",
            Trigger::CatchUp => "catch_up",
            Trigger::Retry => "retry",
            Trigger::Manual => "manual",
        }
    }
}

/// Terminal state of one run (`missed` rows record firings that never ran).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Ok,
    Skipped,
    Failed,
    Cancelled,
    Missed,
}

impl RunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunStatus::Running => "running",
            RunStatus::Ok => "ok",
            RunStatus::Skipped => "skipped",
            RunStatus::Failed => "failed",
            RunStatus::Cancelled => "cancelled",
            RunStatus::Missed => "missed",
        }
    }

    pub fn parse(s: &str) -> RunStatus {
        match s {
            "ok" => RunStatus::Ok,
            "skipped" => RunStatus::Skipped,
            "failed" => RunStatus::Failed,
            "cancelled" => RunStatus::Cancelled,
            "missed" => RunStatus::Missed,
            _ => RunStatus::Running,
        }
    }
}

/// A stored job definition plus its scheduling state.
#[derive(Clone, Debug, Serialize)]
pub struct Job {
    pub id: String,
    pub name: String,
    pub kind: String,
    #[serde(serialize_with = "ser_display")]
    pub schedule: Schedule,
    pub params: Value,
    /// Action classes the job may perform under the user's normal policy.
    /// Anything else always goes to the approval queue.
    pub scope: Vec<String>,
    pub owner: String,
    pub delivery: Delivery,
    pub done_condition: String,
    pub catch_up: CatchUp,
    pub max_failures: u32,
    pub timeout_secs: u64,
    pub enabled: bool,
    pub disabled_reason: String,
    pub created_at: Timestamp,
    pub anchor_at: Timestamp,
    pub next_run_at: Option<Timestamp>,
    pub retry_at: Option<Timestamp>,
    pub consecutive_failures: u32,
    pub last_run_at: Option<Timestamp>,
    pub last_status: String,
}

fn ser_display<S: serde::Serializer>(s: &Schedule, ser: S) -> std::result::Result<S::Ok, S::Error> {
    ser.serialize_str(&s.to_string())
}

/// Input for [`crate::JobStore::add`].
#[derive(Clone, Debug)]
pub struct NewJob {
    pub name: String,
    pub kind: String,
    pub schedule: String,
    pub params: Value,
    pub scope: Vec<String>,
    pub owner: String,
    pub delivery: Delivery,
    pub done_condition: String,
    pub catch_up: CatchUp,
    pub max_failures: u32,
    pub timeout_secs: u64,
    pub enabled: bool,
}

impl NewJob {
    /// A job with conservative defaults: empty scope (every external action
    /// needs approval), digest delivery, no catch-up, 3 failures, 10 minutes.
    pub fn new(name: &str, kind: &str, schedule: &str) -> NewJob {
        NewJob {
            name: name.into(),
            kind: kind.into(),
            schedule: schedule.into(),
            params: Value::Object(Default::default()),
            scope: Vec::new(),
            owner: "user".into(),
            delivery: Delivery::Digest,
            done_condition: String::new(),
            catch_up: CatchUp::None,
            max_failures: 3,
            timeout_secs: 600,
            enabled: true,
        }
    }
}

/// One row of `job_runs`.
#[derive(Clone, Debug, Serialize)]
pub struct JobRun {
    pub id: String,
    pub job_id: String,
    pub trigger: String,
    pub scheduled_for: Option<Timestamp>,
    pub started_at: Timestamp,
    pub finished_at: Option<Timestamp>,
    pub status: RunStatus,
    pub missed: u32,
    pub summary: String,
    pub error: String,
}
