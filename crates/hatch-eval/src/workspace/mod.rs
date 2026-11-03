//! The workspace evaluation: file reading, editing, sandboxed commands and a
//! prompt-injection task, driven through the real chat agent.
//!
//! An evaluation harness plays the user: after every turn it decides the
//! pending approvals strictly by the world's `allow` rules (approve what the
//! task needs, reject everything else), executes approved actions, and tells
//! the agent what happened. No model judgement is involved in approving.

pub mod oracle;
pub mod report;
pub mod runner;

use hatch_core::{Error, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub use report::WorkspaceReport;
pub use runner::{run_workspace_eval, WsMode};

#[derive(Clone, Debug, Deserialize)]
pub struct Persona {
    pub name: String,
    pub email: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Task {
    pub id: String,
    pub prompt: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TasksFile {
    pub name: String,
    pub persona: Persona,
    pub tasks: Vec<Task>,
}

/// One approval rule: every present field must match.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct AllowRule {
    pub kind: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub content_contains: Option<String>,
    #[serde(default)]
    pub command_regex: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct OutsideFile {
    pub path: String,
    pub content: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct WsExpectations {
    #[serde(default)]
    pub answer_contains: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub final_files: BTreeMap<String, String>,
    #[serde(default)]
    pub unchanged_files: Vec<String>,
    #[serde(default)]
    pub must_not_exist: Vec<String>,
    pub allow: Vec<AllowRule>,
    pub outside_file: OutsideFile,
    #[serde(default)]
    pub forbidden_hosts: Vec<String>,
    pub adversarial_task: String,
}

#[derive(Clone, Debug)]
pub struct WorkspaceWorld {
    pub dir: PathBuf,
    pub tasks: TasksFile,
    pub expectations: WsExpectations,
}

impl WorkspaceWorld {
    /// A directory is a workspace world when it has `tasks.json` and `project/`.
    pub fn is_workspace_world(dir: &Path) -> bool {
        dir.join("tasks.json").is_file() && dir.join("project").is_dir()
    }

    pub fn load(dir: &Path) -> Result<WorkspaceWorld> {
        let read = |name: &str| -> Result<String> {
            std::fs::read_to_string(dir.join(name)).map_err(|e| Error::NotFound(format!("{}: {e}", dir.join(name).display())))
        };
        let tasks: TasksFile = serde_json::from_str(&read("tasks.json")?)?;
        let expectations: WsExpectations = serde_json::from_str(&read("expectations.json")?)?;
        for r in &expectations.allow {
            if let Some(re) = &r.command_regex {
                regex::Regex::new(re).map_err(|e| Error::Invalid(format!("allow rule regex: {e}")))?;
            }
        }
        if !tasks.tasks.iter().any(|t| t.id == expectations.adversarial_task) {
            return Err(Error::Invalid(format!("adversarial_task {} is not a task", expectations.adversarial_task)));
        }
        Ok(WorkspaceWorld { dir: dir.to_path_buf(), tasks, expectations })
    }

    pub fn project_dir(&self) -> PathBuf {
        self.dir.join("project")
    }
}

/// Default location of the bundled world (for tests).
pub fn default_world_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../eval/worlds/workspace-basic")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_world_loads() {
        let w = WorkspaceWorld::load(&default_world_dir()).unwrap();
        assert!(WorkspaceWorld::is_workspace_world(&w.dir));
        assert_eq!(w.tasks.tasks.len(), 4);
        assert_eq!(w.expectations.allow.len(), 2);
        assert!(!WorkspaceWorld::is_workspace_world(&default_world_dir().join("../zh-office")));
    }
}
