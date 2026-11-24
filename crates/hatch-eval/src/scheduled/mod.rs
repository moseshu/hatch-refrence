//! User-scheduled tasks (`agent_prompt` jobs) running unattended.
//!
//! The jobs are stored as if the user had created them, then the real
//! scheduler engine fires them on a fake clock through the real
//! [`hatch_workers::PromptWorker`]: one agent turn per firing, under the
//! job's scope, with nobody there to answer questions or approve actions.
//! A timeline interleaves ticks, manual re-runs, new data arriving between
//! runs, and snapshots of delivered outputs.
//!
//! What is checked is resource state, never wording: report files (by item
//! ids), CSV rows (by value), the approval queue, tool calls recorded in
//! the job sessions, and run statuses. Checks are declared in
//! `schedule.json`, so new scenarios need no code.

pub mod oracle;
pub mod report;
pub mod runner;

use hatch_core::time::{parse_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub use report::ScheduledReport;
pub use runner::{run_scheduled_eval, StMode};

#[derive(Clone, Debug, Deserialize)]
pub struct Persona {
    pub name: String,
    pub email: String,
    pub tz: String,
}

/// A standing grant the user gave before the jobs ran.
#[derive(Clone, Debug, Deserialize)]
pub struct GrantSpec {
    pub class: String,
    pub pattern: String,
    #[serde(default)]
    pub allow_when_tainted: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct JobSpec {
    pub name: String,
    pub schedule: String,
    /// Action classes the job may perform without approval (still subject
    /// to policy and grants). Empty: every external action is queued.
    #[serde(default)]
    pub scope: Vec<String>,
    pub prompt: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "do", rename_all = "snake_case")]
pub enum Action {
    /// Let the engine run whatever is due at this time.
    Tick,
    /// Run one job now, as a manual trigger.
    Run { job: String },
    /// New data arrives in the workspace (the user or another tool wrote it).
    Write { path: String, content: String },
    /// Record a workspace file as delivered, for an `unchanged` check.
    Snapshot { path: String },
}

#[derive(Clone, Debug, Deserialize)]
pub struct Step {
    pub at: String,
    #[serde(flatten)]
    pub action: Action,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CheckSpec {
    /// The file exists, names every id in `include` and none in `exclude`.
    FileIds {
        path: String,
        include: Vec<String>,
        #[serde(default)]
        exclude: Vec<String>,
    },
    /// The file still equals its `snapshot` from the timeline.
    Unchanged { path: String },
    /// Exactly one CSV row whose first field is the date `key` (any
    /// separator, e.g. 2026/11/09); `fields` pins numeric columns by index;
    /// `no_numbers` requires the other columns to carry no digits.
    CsvRow {
        path: String,
        key: String,
        #[serde(default)]
        fields: BTreeMap<String, f64>,
        #[serde(default)]
        no_numbers: bool,
    },
    /// Every approval of `kind` from the job has `status` and all of
    /// `to_includes` among its recipients; at least one.
    Approval {
        job: String,
        kind: String,
        #[serde(default)]
        to_includes: Vec<String>,
        status: String,
    },
    /// No approval (title or payload) matches `regex`.
    NoApprovalMatching { regex: String },
    /// No job session called `tool`.
    NoToolCall { tool: String },
    /// Every run the timeline triggered finished `ok`; at least one run.
    RunsOk,
}

#[derive(Clone, Debug, Deserialize)]
pub struct NamedCheck {
    pub name: String,
    #[serde(flatten)]
    pub spec: CheckSpec,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ScheduleFile {
    pub name: String,
    pub persona: Persona,
    /// When the jobs were created (their first firing is computed from it).
    pub created_at: String,
    #[serde(default)]
    pub grants: Vec<GrantSpec>,
    pub jobs: Vec<JobSpec>,
    pub timeline: Vec<Step>,
    pub checks: Vec<NamedCheck>,
}

#[derive(Clone, Debug)]
pub struct ScheduledWorld {
    pub dir: PathBuf,
    pub spec: ScheduleFile,
    pub tz: chrono_tz::Tz,
}

pub(crate) fn ts(s: &str) -> Result<Timestamp> {
    parse_rfc3339(s).map_err(|e| Error::Invalid(format!("{s}: {e}")))
}

impl ScheduledWorld {
    /// A directory is a scheduled-task world when it has `schedule.json` and `project/`.
    pub fn is_scheduled_world(dir: &Path) -> bool {
        dir.join("schedule.json").is_file() && dir.join("project").is_dir()
    }

    pub fn load(dir: &Path) -> Result<ScheduledWorld> {
        let path = dir.join("schedule.json");
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| Error::NotFound(format!("{}: {e}", path.display())))?;
        let spec: ScheduleFile = serde_json::from_str(&raw)?;
        let tz: chrono_tz::Tz = spec
            .persona
            .tz
            .parse()
            .map_err(|e| Error::Invalid(format!("persona.tz {}: {e}", spec.persona.tz)))?;
        let mut last = ts(&spec.created_at)?;
        for g in &spec.grants {
            hatch_policy::ActionClass::parse(&g.class)?;
        }
        let mut jobs = BTreeSet::new();
        for j in &spec.jobs {
            if !jobs.insert(j.name.clone()) {
                return Err(Error::Invalid(format!("duplicate job {}", j.name)));
            }
            for c in &j.scope {
                hatch_policy::ActionClass::parse(c)?;
            }
        }
        let mut snapshots = BTreeSet::new();
        for s in &spec.timeline {
            let at = ts(&s.at)?;
            if at <= last {
                return Err(Error::Invalid(format!("timeline step {} is not after the previous one", s.at)));
            }
            last = at;
            match &s.action {
                Action::Run { job } if !jobs.contains(job) => {
                    return Err(Error::Invalid(format!("timeline runs unknown job {job}")));
                }
                Action::Snapshot { path } => {
                    snapshots.insert(path.clone());
                }
                _ => {}
            }
        }
        for c in &spec.checks {
            match &c.spec {
                CheckSpec::Unchanged { path } if !snapshots.contains(path) => {
                    return Err(Error::Invalid(format!("check {}: no snapshot of {path}", c.name)));
                }
                CheckSpec::Approval { job, .. } if !jobs.contains(job) => {
                    return Err(Error::Invalid(format!("check {}: unknown job {job}", c.name)));
                }
                CheckSpec::NoApprovalMatching { regex } => {
                    regex::Regex::new(regex)
                        .map_err(|e| Error::Invalid(format!("check {}: regex: {e}", c.name)))?;
                }
                CheckSpec::CsvRow { fields, .. } => {
                    for k in fields.keys() {
                        k.parse::<usize>()
                            .map_err(|_| Error::Invalid(format!("check {}: field index {k}", c.name)))?;
                    }
                }
                _ => {}
            }
        }
        Ok(ScheduledWorld {
            dir: dir.to_path_buf(),
            spec,
            tz,
        })
    }

    pub fn project_dir(&self) -> PathBuf {
        self.dir.join("project")
    }
}

/// Default location of the bundled world (for tests).
pub fn default_world_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../eval/worlds/scheduled-tasks")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_world_loads() {
        let w = ScheduledWorld::load(&default_world_dir()).unwrap();
        assert!(ScheduledWorld::is_scheduled_world(&w.dir));
        assert_eq!(w.spec.jobs.len(), 3);
        assert_eq!(w.spec.checks.len(), 9);
        assert!(!ScheduledWorld::is_scheduled_world(
            &default_world_dir().join("../scheduler-basic")
        ));
    }
}
