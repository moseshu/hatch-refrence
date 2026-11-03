//! The delegation evaluation: sub-agents splitting work, cancellation of a
//! running tree, the depth limit, and a sub-agent meeting injected content.
//!
//! A harness plays the user: it presses "Ctrl-C" at a fixed point (after the
//! N-th tool call made by a sub-agent, or after a timeout), rejects every
//! pending approval, and checks the database, the workspace and the process
//! table. No model judgement is involved in scoring.

pub mod oracle;
pub mod report;
pub mod runner;

use hatch_core::{Error, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub use report::DelegateReport;
pub use runner::{run_delegate_eval, DgMode};

#[derive(Clone, Debug, Deserialize)]
pub struct Persona {
    pub name: String,
    pub email: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct GrantSpec {
    pub class: String,
    pub pattern: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SplitTask {
    pub prompt: String,
    /// Correct total number of TODO lines.
    pub total: u64,
    /// The service with the most TODOs.
    pub top_service: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CancelTask {
    pub prompt: String,
    pub follow_up: String,
    /// Press Ctrl-C once sub-agents have started this many tool calls …
    pub cancel_after_subagent_tool_calls: usize,
    /// … or after this many seconds, whichever comes first.
    pub cancel_after_secs: u64,
    /// Argument of the long `sleep` that must not survive the cancellation.
    pub process_marker: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DepthTask {
    pub prompt: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct InjectionTask {
    pub prompt: String,
    pub must_not_exist: Vec<String>,
    pub unchanged_files: Vec<String>,
    pub forbidden_hosts: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DelegateSpec {
    pub name: String,
    pub persona: Persona,
    pub max_depth: usize,
    pub max_parallel: usize,
    #[serde(default)]
    pub grants: Vec<GrantSpec>,
    pub split: SplitTask,
    pub cancel: CancelTask,
    pub depth: DepthTask,
    pub injection: InjectionTask,
}

#[derive(Clone, Debug)]
pub struct DelegateWorld {
    pub dir: PathBuf,
    pub spec: DelegateSpec,
}

impl DelegateWorld {
    /// A directory is a delegation world when it has `delegate.json` and `project/`.
    pub fn is_delegate_world(dir: &Path) -> bool {
        dir.join("delegate.json").is_file() && dir.join("project").is_dir()
    }

    pub fn load(dir: &Path) -> Result<DelegateWorld> {
        let path = dir.join("delegate.json");
        let text = std::fs::read_to_string(&path)
            .map_err(|e| Error::NotFound(format!("{}: {e}", path.display())))?;
        let spec: DelegateSpec = serde_json::from_str(&text)?;
        for g in &spec.grants {
            hatch_policy::ActionClass::parse(&g.class)
                .map_err(|e| Error::Invalid(format!("grant: {e}")))?;
        }
        if spec.max_depth == 0 {
            return Err(Error::Invalid(
                "max_depth must be ≥ 1 for a delegation world".into(),
            ));
        }
        Ok(DelegateWorld {
            dir: dir.to_path_buf(),
            spec,
        })
    }

    pub fn project_dir(&self) -> PathBuf {
        self.dir.join("project")
    }
}

/// Default location of the bundled world.
pub fn default_world_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../eval/worlds/delegate-basic")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_world_loads() {
        let w = DelegateWorld::load(&default_world_dir()).unwrap();
        assert!(DelegateWorld::is_delegate_world(&w.dir));
        assert_eq!(w.spec.max_depth, 1);
        assert_eq!(w.spec.grants[0].pattern, "sh slow.sh");
        assert!(!DelegateWorld::is_delegate_world(
            &default_world_dir().join("../workspace-basic")
        ));
    }
}
