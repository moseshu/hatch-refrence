//! The scheduler evaluation: DST, downtime, failures, timeouts, graceful
//! stop, the daily brief and the scope of scheduled outbound actions.
//!
//! Timing scenarios run on an injected fake clock (days pass in
//! milliseconds) and use closure handlers — no model involved. The brief and
//! outbound scenarios use the real workers with a scripted model (pipeline)
//! or the configured one (live). Scoring is done by code only.

pub mod brief;
pub mod outbound;
pub mod report;
pub mod timing;

use hatch_core::time::{parse_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use hatch_workers::RouterFactory;
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub use report::SchedulerReport;

#[derive(Clone, Debug, Deserialize)]
pub struct Persona {
    pub name: String,
    pub email: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DstSpec {
    pub schedule: String,
    pub from: String,
    pub to: String,
    pub expected_utc: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DowntimeSpec {
    pub schedule: String,
    pub created: String,
    pub down_from: String,
    pub down_to: String,
    pub resume_until: String,
    pub expected_missed: u32,
    pub runs_before: usize,
    pub runs_after: usize,
}

#[derive(Clone, Debug, Deserialize)]
pub struct FailureSpec {
    pub max_failures: u32,
    pub error: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BriefMail {
    pub id: String,
    pub from: String,
    #[serde(default)]
    pub name: String,
    pub subject: String,
    pub category: String,
    pub priority: String,
    pub summary: String,
    pub needs_reply: bool,
    pub suspicious: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BriefEvent {
    pub uid: String,
    pub title: String,
    pub start: String,
    pub minutes: i64,
    #[serde(default)]
    pub location: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BriefSpec {
    pub now: String,
    pub last_triage_hours_ago: i64,
    pub mails: Vec<BriefMail>,
    pub events: Vec<BriefEvent>,
    pub approvals: Vec<String>,
    pub reminders: Vec<String>,
    pub must_mention: Vec<String>,
    pub must_not_mention: Vec<String>,
    pub fabricated: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct OutboundSpec {
    pub mail: hatch_connectors::mail::fixture::FixtureMail,
    pub grant_pattern: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SchedulerSpec {
    pub name: String,
    pub persona: Persona,
    pub timezone: String,
    pub tick_minutes: i64,
    pub dst_fall: DstSpec,
    pub dst_spring: DstSpec,
    pub downtime: DowntimeSpec,
    pub failure: FailureSpec,
    pub brief: BriefSpec,
    pub outbound: OutboundSpec,
}

#[derive(Clone, Debug)]
pub struct SchedulerWorld {
    pub dir: PathBuf,
    pub spec: SchedulerSpec,
    pub tz: chrono_tz::Tz,
}

impl SchedulerWorld {
    pub fn is_scheduler_world(dir: &Path) -> bool {
        dir.join("scheduler.json").is_file()
    }

    pub fn load(dir: &Path) -> Result<SchedulerWorld> {
        let path = dir.join("scheduler.json");
        let text = std::fs::read_to_string(&path)
            .map_err(|e| Error::NotFound(format!("{}: {e}", path.display())))?;
        let spec: SchedulerSpec = serde_json::from_str(&text)?;
        let tz = hatch_core::time::parse_tz(&spec.timezone)?;
        if spec.tick_minutes <= 0 {
            return Err(Error::Invalid("tick_minutes must be positive".into()));
        }
        Ok(SchedulerWorld {
            dir: dir.to_path_buf(),
            spec,
            tz,
        })
    }
}

pub fn ts(s: &str) -> Result<Timestamp> {
    parse_rfc3339(s)
}

/// Default location of the bundled world.
pub fn default_world_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../eval/worlds/scheduler-basic")
}

pub enum SchedMode {
    /// Scripted models only.
    Pipeline,
    /// The configured model for the brief and triage; a fresh router per run.
    Live {
        factory: RouterFactory,
        label: String,
    },
}

/// Run every scenario and score it.
pub fn run_scheduler_eval(world: &SchedulerWorld, mode: SchedMode) -> Result<SchedulerReport> {
    let mut checks = Vec::new();
    let mut notes = Vec::new();
    timing::run(world, &mut checks, &mut notes)?;
    let (live, factory, model) = match &mode {
        SchedMode::Pipeline => (false, None, "script".to_string()),
        SchedMode::Live { factory, label } => (true, Some(factory.clone()), label.clone()),
    };
    let brief = brief::run(world, factory.clone(), live, &mut checks)?;
    let outbound = outbound::run(world, factory, &mut checks)?;
    Ok(SchedulerReport {
        mode: if live {
            "live".into()
        } else {
            "pipeline".into()
        },
        model,
        world: world.spec.name.clone(),
        timezone: world.spec.timezone.clone(),
        checks,
        notes,
        brief,
        outbound,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_world_loads_and_passes_in_pipeline_mode() {
        let w = SchedulerWorld::load(&default_world_dir()).unwrap();
        assert!(SchedulerWorld::is_scheduler_world(&w.dir));
        let r = run_scheduler_eval(&w, SchedMode::Pipeline).unwrap();
        let failed: Vec<_> = r.checks.iter().filter(|c| !c.passed).collect();
        assert!(failed.is_empty(), "{failed:#?}\n{}", r.render_markdown());
    }
}
