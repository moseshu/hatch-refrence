//! Real web: how the agent finds, reads and cites information on the
//! live internet.
//!
//! `--live` runs the product's own `web_fetch` (and `web_search` when a
//! search backend is configured) against the real network. Pipeline mode
//! serves the same URLs from local fixture pages so the harness and the
//! checks can be tested offline; it says nothing about the real web.
//!
//! Every fetch and search is recorded (see [`net`]): which URL, whether it
//! worked, where it ended up, and the text the agent saw. Checks only look
//! at resource state — output files and that record — never at wording:
//! the answer is in the file *and* came from a page on the right site, a
//! cited link's site was really opened, a value that changes over time
//! matches the source when the run is scored, a dead link or unreachable
//! host is reported without invented content and without endless retries.

pub mod net;
pub mod oracle;
pub mod report;
pub mod runner;

use hatch_core::time::{parse_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use report::WebReport;
pub use runner::{run_web_eval, WlMode};

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

#[derive(Clone, Debug, Deserialize)]
pub struct Task {
    pub id: String,
    pub prompt: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TaskFile {
    pub task: String,
    pub path: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CheckSpec {
    /// The file contains every needle, and the task fetched at least one
    /// page from `host` (or a subdomain) successfully.
    AnswerFrom {
        task: String,
        path: String,
        needles: Vec<String>,
        host: String,
    },
    /// A value that changes over time: when the run is scored, the harness
    /// reads `url` itself and takes the JSON string at `pointer`; the file
    /// must contain it, and the task must have fetched `host` successfully.
    LiveValue {
        task: String,
        path: String,
        url: String,
        pointer: String,
        host: String,
    },
    /// Each file cites at least one http(s) link, and every cited link's
    /// site was opened successfully in that task.
    CitedFetched { items: Vec<TaskFile> },
    /// Every number in the file is in `allowed` (dates, whole numbers up to
    /// 31 and the current year aside). A missing file passes when
    /// `allow_absent`.
    NumbersAllowed {
        path: String,
        allowed: Vec<f64>,
        #[serde(default)]
        allow_absent: bool,
    },
    /// The file is missing, or nothing in it matches the regex `pattern`.
    AbsentOrLacks { path: String, pattern: String },
    /// `web_fetch` calls in the task whose URL contains `url_contains`:
    /// at least `min`, at most `max`.
    FetchCalls {
        task: String,
        url_contains: String,
        min: usize,
        max: usize,
    },
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
pub struct WebFile {
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
pub struct WebWorld {
    pub dir: PathBuf,
    pub spec: WebFile,
    pub tz: chrono_tz::Tz,
}

pub(crate) fn ts(s: &str) -> Result<Timestamp> {
    parse_rfc3339(s).map_err(|e| Error::Invalid(format!("{s}: {e}")))
}

impl WebWorld {
    /// A directory is a real-web world when it has `web.json` and `project/`.
    pub fn is_web_world(dir: &Path) -> bool {
        dir.join("web.json").is_file() && dir.join("project").is_dir()
    }

    pub fn load(dir: &Path) -> Result<WebWorld> {
        let path = dir.join("web.json");
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| Error::NotFound(format!("{}: {e}", path.display())))?;
        let probe: serde_json::Value = serde_json::from_str(&raw)?;
        let tz_name = probe["persona"]["tz"].as_str().unwrap_or("").to_string();
        let tz: chrono_tz::Tz = tz_name
            .parse()
            .map_err(|e| Error::Invalid(format!("persona.tz {tz_name}: {e}")))?;
        let base = crate::dates::base_date(&tz)?;
        let spec: WebFile = serde_json::from_str(&crate::dates::render(&raw, base)?)?;
        ts(&spec.now)?;
        let mut ids = BTreeSet::new();
        for t in &spec.tasks {
            if !ids.insert(t.id.clone()) {
                return Err(Error::Invalid(format!("duplicate task {}", t.id)));
            }
        }
        for c in &spec.checks {
            let refs: Vec<&String> = match &c.spec {
                CheckSpec::AnswerFrom { task, .. }
                | CheckSpec::LiveValue { task, .. }
                | CheckSpec::FetchCalls { task, .. } => vec![task],
                CheckSpec::CitedFetched { items } => items.iter().map(|i| &i.task).collect(),
                CheckSpec::NoQuestions { tasks } => tasks.iter().collect(),
                CheckSpec::NumbersAllowed { .. } | CheckSpec::AbsentOrLacks { .. } => vec![],
            };
            if let Some(t) = refs.into_iter().find(|t| !ids.contains(*t)) {
                return Err(Error::Invalid(format!("check {}: unknown task {t}", c.name)));
            }
            if let CheckSpec::AbsentOrLacks { pattern, .. } = &c.spec {
                regex::Regex::new(pattern)
                    .map_err(|e| Error::Invalid(format!("check {}: pattern: {e}", c.name)))?;
            }
        }
        Ok(WebWorld {
            dir: dir.to_path_buf(),
            spec,
            tz,
        })
    }

    /// Load and validate fixed benchmark values before an offline run starts.
    pub(crate) fn load_fixture_truth(&self) -> Result<(Arc<net::Fixtures>, net::Truth)> {
        let pages = Arc::new(net::Fixtures::load(&self.fixtures_dir())?);
        let truth = net::Truth::Fixture(pages.clone());
        for check in &self.spec.checks {
            if let CheckSpec::LiveValue { url, pointer, .. } = &check.spec {
                truth.json_value(url, pointer).map_err(|error| {
                    Error::Invalid(format!("check {}: invalid offline benchmark: {error}", check.name))
                })?;
            }
        }
        Ok((pages, truth))
    }

    pub fn project_dir(&self) -> PathBuf {
        self.dir.join("project")
    }

    /// Offline pages for pipeline mode (`fixtures/pages.json`).
    pub fn fixtures_dir(&self) -> PathBuf {
        self.dir.join("fixtures")
    }
}

/// Default location of the bundled world (for tests).
pub fn default_world_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../eval/worlds/web-live")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_world_loads() {
        let w = WebWorld::load(&default_world_dir()).unwrap();
        assert!(WebWorld::is_web_world(&w.dir));
        assert_eq!(w.spec.tasks.len(), 6);
        assert_eq!(w.spec.checks.len(), 10);
        let pages = net::Fixtures::load(&w.fixtures_dir()).unwrap();
        // Every URL a LiveValue check reads must be served offline too.
        for c in &w.spec.checks {
            if let CheckSpec::LiveValue { url, .. } = &c.spec {
                assert!(pages.get(url).is_some(), "fixture missing for {url}");
            }
        }
    }
}
