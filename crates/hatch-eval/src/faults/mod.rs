//! Tool faults: how the agent behaves when its tools fail.
//!
//! Real tools are wrapped (see [`inject`]) so chosen calls fail the way
//! real systems do: a transient error that clears on retry, a permission
//! error that never clears, an outage that persists, a write that lands
//! but loses its response (`UnknownOutcome`, which the product already
//! routes to "needs review"). Large results page naturally (`fs_read`
//! shows 400 lines with a "N lines total" header).
//!
//! What is checked is resource state: output files (ids, allowed numbers,
//! a line count), how many calls hit a failing source, and whether the
//! agent asked the user about problems it could handle itself. The
//! harness approves workspace writes under allowed paths, reports each
//! execution's real outcome back, answers questions neutrally, and
//! rejects everything else.

pub mod inject;
pub mod oracle;
pub mod report;
pub mod runner;

use hatch_core::time::{parse_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub use report::FaultsReport;
pub use runner::{run_faults_eval, FtMode};

#[derive(Clone, Debug, Deserialize)]
pub struct Persona {
    pub name: String,
    pub email: String,
    pub tz: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AllowRule {
    pub kind: String,
    #[serde(default)]
    pub path_prefix: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FaultKind {
    /// A service error worth retrying (`Error::Connector`).
    Unavailable,
    /// A permission error that retrying cannot fix (`Error::Denied`).
    Denied,
}

/// Fail wrapped-tool calls whose arguments mention `path`.
#[derive(Clone, Debug, Deserialize)]
pub struct ToolFault {
    /// Tool names this applies to; empty = every wrapped tool.
    #[serde(default)]
    pub tools: Vec<String>,
    pub path: String,
    /// Fail only the first N matching calls; absent = every call (and the
    /// file's content is kept out of other tools' results meanwhile).
    #[serde(default)]
    pub first: Option<usize>,
    pub kind: FaultKind,
    pub message: String,
}

/// Let the first `first` writes to `path` land, then report a lost response.
#[derive(Clone, Debug, Deserialize)]
pub struct WriteFault {
    pub path: String,
    pub first: usize,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Task {
    pub id: String,
    pub prompt: String,
    #[serde(default)]
    pub tool_faults: Vec<ToolFault>,
    #[serde(default)]
    pub write_faults: Vec<WriteFault>,
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
    /// Every number in the file is in `allowed`, apart from dates and
    /// small whole numbers (≤ 31: days, months, counts). A missing file
    /// passes when `allow_absent`.
    NumbersAllowed {
        path: String,
        allowed: Vec<f64>,
        #[serde(default)]
        allow_absent: bool,
    },
    /// `needle` occurs exactly `n` times in the file.
    Count { path: String, needle: String, n: usize },
    /// At most `max` wrapped-tool calls in the task mentioned `path`.
    CallsAtMost { task: String, path: String, max: usize },
    /// None of these tasks called `ask_user`.
    NoQuestions { tasks: Vec<String> },
}

#[derive(Clone, Debug, Deserialize)]
pub struct NamedCheck {
    pub name: String,
    #[serde(flatten)]
    pub spec: CheckSpec,
}

#[derive(Clone, Debug, Deserialize)]
pub struct FaultsFile {
    pub name: String,
    pub persona: Persona,
    /// Run clock; may use date templates (see `crate::dates`).
    pub now: String,
    #[serde(default)]
    pub allow: Vec<AllowRule>,
    pub tasks: Vec<Task>,
    pub checks: Vec<NamedCheck>,
}

#[derive(Clone, Debug)]
pub struct FaultsWorld {
    pub dir: PathBuf,
    pub spec: FaultsFile,
    pub tz: chrono_tz::Tz,
}

pub(crate) fn ts(s: &str) -> Result<Timestamp> {
    parse_rfc3339(s).map_err(|e| Error::Invalid(format!("{s}: {e}")))
}

impl FaultsWorld {
    /// A directory is a tool-faults world when it has `faults.json` and `project/`.
    pub fn is_faults_world(dir: &Path) -> bool {
        dir.join("faults.json").is_file() && dir.join("project").is_dir()
    }

    pub fn load(dir: &Path) -> Result<FaultsWorld> {
        let path = dir.join("faults.json");
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| Error::NotFound(format!("{}: {e}", path.display())))?;
        let probe: serde_json::Value = serde_json::from_str(&raw)?;
        let tz_name = probe["persona"]["tz"].as_str().unwrap_or("").to_string();
        let tz: chrono_tz::Tz = tz_name
            .parse()
            .map_err(|e| Error::Invalid(format!("persona.tz {tz_name}: {e}")))?;
        let base = crate::dates::base_date(&tz)?;
        let spec: FaultsFile = serde_json::from_str(&crate::dates::render(&raw, base)?)?;
        ts(&spec.now)?;
        let mut ids = BTreeSet::new();
        for t in &spec.tasks {
            if !ids.insert(t.id.clone()) {
                return Err(Error::Invalid(format!("duplicate task {}", t.id)));
            }
            if t.tool_faults.iter().any(|f| f.first == Some(0)) || t.write_faults.iter().any(|f| f.first == 0) {
                return Err(Error::Invalid(format!("task {}: `first` must be at least 1", t.id)));
            }
        }
        for c in &spec.checks {
            let refs: Vec<&String> = match &c.spec {
                CheckSpec::CallsAtMost { task, .. } => vec![task],
                CheckSpec::NoQuestions { tasks } => tasks.iter().collect(),
                _ => vec![],
            };
            if let Some(t) = refs.into_iter().find(|t| !ids.contains(*t)) {
                return Err(Error::Invalid(format!("check {}: unknown task {t}", c.name)));
            }
        }
        Ok(FaultsWorld {
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
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../eval/worlds/tool-faults")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_world_loads() {
        let w = FaultsWorld::load(&default_world_dir()).unwrap();
        assert!(FaultsWorld::is_faults_world(&w.dir));
        assert_eq!(w.spec.tasks.len(), 5);
        assert_eq!(w.spec.checks.len(), 8);
        let lines = std::fs::read_to_string(w.project_dir().join("data/orders-nov.csv"))
            .unwrap()
            .lines()
            .count();
        assert!(lines > 400, "the paging task needs more than one fs_read page: {lines}");
    }
}
