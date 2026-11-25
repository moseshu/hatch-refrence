//! Diagnostic traces and the run manifest for memory evaluations.
//!
//! A failed check says *that* a task went wrong; diagnosing *why* needs
//! the raw evidence: the context the model actually received, every tool
//! call with its arguments and result, and the answers. Summaries are not
//! enough (they compress away the details a diagnosis needs), so every
//! run with `--out` keeps the full trace next to the report:
//!
//! ```text
//! <out>/run.json                 manifest: commit, model, recall mode,
//!                                fixture fingerprint, usage, pass counts
//! <out>/traces/NN-<session>.json one file per session, turn by turn
//! <out>/contexts/<digest>.txt    each distinct system context, once
//! ```
//!
//! Everything written is passed through `hatch_core::secrets::redact`.
//! The files can hold task content; for held-out packs `<out>` must be a
//! private directory, and only the summary leaves it.

use hatch_agent::StepEvent;
use hatch_core::secrets::redact;
use hatch_core::Result;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TraceEvent {
    Call { step: usize, tool: String, args: String },
    Result { step: usize, tool: String, trust: String, content: String },
    Answer { step: usize, text: String },
}

impl TraceEvent {
    pub fn from_step(e: &StepEvent) -> Vec<TraceEvent> {
        match e {
            StepEvent::Decision { step, calls, .. } => calls
                .iter()
                .map(|c| TraceEvent::Call {
                    step: *step,
                    tool: c.name.clone(),
                    args: redact(&c.arguments.to_string()),
                })
                .collect(),
            StepEvent::Result { step, tool, content, trust, .. } => vec![TraceEvent::Result {
                step: *step,
                tool: tool.clone(),
                trust: format!("{trust:?}"),
                content: redact(content),
            }],
            StepEvent::Answer { step, text } => vec![TraceEvent::Answer {
                step: *step,
                text: redact(text),
            }],
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct TraceTurn {
    pub input: String,
    /// Digest of each main-workload request's system context, in order;
    /// the text is in `contexts/<digest>.txt`.
    pub contexts: Vec<String>,
    pub events: Vec<TraceEvent>,
    pub answer: String,
    pub stop: String,
    pub steps: usize,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SessionTrace {
    pub day: String,
    pub session: String,
    pub turns: Vec<TraceTurn>,
}

#[derive(Clone, Debug, Default)]
pub struct Diagnostics {
    pub sessions: Vec<SessionTrace>,
    /// digest → redacted system context.
    pub contexts: BTreeMap<String, String>,
}

impl Diagnostics {
    /// Store a system context once; its digest for the trace.
    pub fn context(&mut self, system: &str) -> String {
        let text = redact(system);
        let digest = hatch_core::ids::digest(&[&text])[..16].to_string();
        self.contexts.entry(digest.clone()).or_insert(text);
        digest
    }

    pub fn write(&self, dir: &Path) -> Result<()> {
        let traces = dir.join("traces");
        let contexts = dir.join("contexts");
        std::fs::create_dir_all(&traces)?;
        std::fs::create_dir_all(&contexts)?;
        for (i, s) in self.sessions.iter().enumerate() {
            let name: String = s
                .session
                .chars()
                .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
                .collect();
            std::fs::write(
                traces.join(format!("{:02}-{name}.json", i + 1)),
                serde_json::to_string_pretty(s)?,
            )?;
        }
        for (digest, text) in &self.contexts {
            std::fs::write(contexts.join(format!("{digest}.txt")), text)?;
        }
        Ok(())
    }
}

/// What a run was: enough to reproduce it or line it up with others.
#[derive(Clone, Debug, Serialize)]
pub struct RunManifest {
    pub world: String,
    /// SHA-256 of the world directory (paths and contents).
    pub fixture_fingerprint: String,
    pub heldout: bool,
    pub mode: String,
    pub model: String,
    pub recall: String,
    /// `git rev-parse HEAD` where the run started, if available.
    pub commit: Option<String>,
    /// Tracked files had uncommitted changes.
    pub dirty: Option<bool>,
    pub hatch_version: String,
    pub started_at: String,
    pub finished_at: String,
    pub checks_passed: usize,
    pub checks_total: usize,
    pub usage: crate::report::Usage,
    pub extract_usage: crate::report::Usage,
}

/// Current commit and dirty flag of the git checkout at `dir`, when git
/// is available and `dir` is inside a repository.
pub fn git_state(dir: &Path) -> (Option<String>, Option<bool>) {
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let commit = run(&["rev-parse", "HEAD"]).filter(|c| !c.is_empty());
    let dirty = commit
        .as_ref()
        .and_then(|_| run(&["status", "--porcelain", "--untracked-files=no"]))
        .map(|s| !s.is_empty());
    (commit, dirty)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contexts_are_stored_once_and_secrets_redacted() {
        let mut d = Diagnostics::default();
        let a = d.context("profile line\napi key sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
        let b = d.context("profile line\napi key sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
        assert_eq!(a, b);
        assert_eq!(d.contexts.len(), 1);
        assert!(!d.contexts[&a].contains("sk-ant-api03-AAAA"), "{}", d.contexts[&a]);

        d.sessions.push(SessionTrace {
            day: "D1".into(),
            session: "s/1".into(),
            turns: vec![TraceTurn { contexts: vec![a.clone()], ..Default::default() }],
        });
        let tmp = tempfile::tempdir().unwrap();
        d.write(tmp.path()).unwrap();
        assert!(tmp.path().join("traces/01-s_1.json").is_file());
        assert!(tmp.path().join(format!("contexts/{a}.txt")).is_file());
    }
}
