//! Cross-day memory evaluation, driven through the real chat agent.
//!
//! A memory world is a sequence of simulated days. Each day holds chat
//! sessions; between days the nightly `memory_extract` worker runs, just as
//! the daemon's default `daily 04:00` job does. Facts the user states in
//! passing on one day must shape what the agent does on a later day, in a
//! different session, without being repeated.
//!
//! What is checked is resource state, never the agent's wording: draft
//! payloads (recipients, event start), files written to the workspace, the
//! memory bank itself (no trusted claim from injected content, no secret
//! anywhere). Checks are declared in `memory.json`, so new scenarios need
//! no code.
//!
//! The harness plays the user mechanically: workspace writes under allowed
//! paths are approved and executed, outbound drafts (mail, events) stay
//! pending for inspection, anything else is rejected.

pub mod oracle;
pub mod report;
pub mod runner;

use hatch_core::time::{parse_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub use report::MemoryReport;
pub use runner::{run_memory_eval, MemMode};

#[derive(Clone, Debug, Deserialize)]
pub struct Persona {
    pub name: String,
    pub email: String,
    pub tz: String,
}

/// A memory that already exists before the first simulated day.
#[derive(Clone, Debug, Deserialize)]
pub struct SeedClaim {
    pub subject: String,
    pub text: String,
    /// RFC3339; the claim's creation time. The fixed profile takes the
    /// oldest tagged claims, so give every seeded claim a distinct time.
    pub at: String,
    /// Memory tags (`preference`, `profile`, `style` put a claim in the
    /// fixed profile, oldest first, up to 12).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Ranking salience (0–1); the store's default when absent.
    #[serde(default)]
    pub salience: Option<f64>,
    /// Name a `targets` entry refers to.
    #[serde(default)]
    pub key: Option<String>,
}

/// A seeded memory a session needs: the report shows whether it reached
/// the model's context (fixed profile or automatic recall), whether a
/// memory tool found it, and what that cost. Observations, not checks:
/// whether recall *should* fetch it depends on the recall mode under test.
#[derive(Clone, Debug, Deserialize)]
pub struct Target {
    /// `key` of a seed claim.
    pub key: String,
    pub session: String,
    /// Free-form label of the case (e.g. 词面交集 / 换了说法 / 完全没提).
    pub condition: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Session {
    pub id: String,
    /// User messages, sent in order within one chat session.
    pub turns: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Day {
    pub label: String,
    /// RFC3339; the clock when the day's first session starts.
    pub at: String,
    pub sessions: Vec<Session>,
}

/// Workspace writes the harness approves. Only `write_file` is ever approved.
#[derive(Clone, Debug, Deserialize)]
pub struct AllowRule {
    pub kind: String,
    #[serde(default)]
    pub path_prefix: Option<String>,
}

fn one() -> usize {
    1
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CheckSpec {
    /// Every `send_email` draft of the session has all of `to_includes`
    /// and none of `to_excludes` among its recipients; at least one draft.
    EmailDraft {
        session: String,
        #[serde(default)]
        to_includes: Vec<String>,
        #[serde(default)]
        to_excludes: Vec<String>,
    },
    /// Every event draft of the session starts at `local_start`
    /// (`YYYY-MM-DDTHH:MM` in the persona's time zone) and its location
    /// contains each of `location_includes`; at least one draft.
    EventDraft {
        session: String,
        local_start: String,
        #[serde(default)]
        location_includes: Vec<String>,
    },
    /// Every `send_email` draft of the session: the body contains all of
    /// `includes` and none of `excludes`, and its last non-empty line is
    /// `last_line` when given; at least one draft.
    EmailBody {
        session: String,
        #[serde(default)]
        includes: Vec<String>,
        #[serde(default)]
        excludes: Vec<String>,
        #[serde(default)]
        last_line: Option<String>,
    },
    /// The workspace file exists, names at least `min_allowed` of
    /// `allowed` and none of `forbidden`.
    FileNames {
        path: String,
        allowed: Vec<String>,
        forbidden: Vec<String>,
        #[serde(default = "one")]
        min_allowed: usize,
    },
    /// No current claim that is not marked untrusted contains any of `any`.
    NoTrustedClaim { any: Vec<String> },
    /// The session created no approval of any kind.
    NoApprovals { session: String },
    /// None of `strings` appears in any claim, workspace file or approval.
    Nowhere { strings: Vec<String> },
}

impl CheckSpec {
    fn session(&self) -> Option<&str> {
        match self {
            CheckSpec::EmailDraft { session, .. }
            | CheckSpec::EmailBody { session, .. }
            | CheckSpec::EventDraft { session, .. }
            | CheckSpec::NoApprovals { session } => Some(session),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct NamedCheck {
    pub name: String,
    #[serde(flatten)]
    pub spec: CheckSpec,
}

#[derive(Clone, Debug, Deserialize)]
pub struct MemoryFile {
    pub name: String,
    pub persona: Persona,
    #[serde(default)]
    pub seed_claims: Vec<SeedClaim>,
    #[serde(default)]
    pub targets: Vec<Target>,
    /// Run the nightly extraction worker between days.
    #[serde(default)]
    pub night_extract: bool,
    #[serde(default)]
    pub allow: Vec<AllowRule>,
    pub days: Vec<Day>,
    pub checks: Vec<NamedCheck>,
}

#[derive(Clone, Debug)]
pub struct MemoryWorld {
    pub dir: PathBuf,
    pub spec: MemoryFile,
    pub tz: chrono_tz::Tz,
}

fn ts(s: &str, what: &str) -> Result<Timestamp> {
    parse_rfc3339(s).map_err(|e| Error::Invalid(format!("{what}: {s}: {e}")))
}

impl MemoryWorld {
    /// A directory is a memory world when it has `memory.json` and `project/`.
    pub fn is_memory_world(dir: &Path) -> bool {
        dir.join("memory.json").is_file() && dir.join("project").is_dir()
    }

    pub fn load(dir: &Path) -> Result<MemoryWorld> {
        let path = dir.join("memory.json");
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| Error::NotFound(format!("{}: {e}", path.display())))?;
        let spec: MemoryFile = serde_json::from_str(&raw)?;
        let tz: chrono_tz::Tz = spec
            .persona
            .tz
            .parse()
            .map_err(|e| Error::Invalid(format!("persona.tz {}: {e}", spec.persona.tz)))?;
        if spec.days.is_empty() {
            return Err(Error::Invalid("memory world has no days".into()));
        }
        let mut keys = BTreeSet::new();
        let mut times = BTreeSet::new();
        for c in &spec.seed_claims {
            let at = ts(&c.at, "seed_claims.at")?;
            if !c.tags.is_empty() && !times.insert(at) {
                return Err(Error::Invalid(format!(
                    "seed claim {} shares its time with another tagged claim; profile order would be ambiguous",
                    c.subject
                )));
            }
            if c.salience.is_some_and(|s| !(0.0..=1.0).contains(&s)) {
                return Err(Error::Invalid(format!("seed claim {}: salience must be 0–1", c.subject)));
            }
            if let Some(k) = &c.key {
                if !keys.insert(k.clone()) {
                    return Err(Error::Invalid(format!("duplicate seed key {k}")));
                }
            }
        }
        let mut ids = BTreeSet::new();
        let mut last: Option<Timestamp> = None;
        for d in &spec.days {
            let at = ts(&d.at, "days.at")?;
            if last.is_some_and(|l| at <= l) {
                return Err(Error::Invalid(format!("day {} is not after the previous day", d.label)));
            }
            last = Some(at);
            for s in &d.sessions {
                if s.turns.is_empty() {
                    return Err(Error::Invalid(format!("session {} has no turns", s.id)));
                }
                if !ids.insert(s.id.clone()) {
                    return Err(Error::Invalid(format!("duplicate session id {}", s.id)));
                }
            }
        }
        for t in &spec.targets {
            if !keys.contains(&t.key) {
                return Err(Error::Invalid(format!("target {}: no seed claim has this key", t.key)));
            }
            if !ids.contains(&t.session) {
                return Err(Error::Invalid(format!("target {}: unknown session {}", t.key, t.session)));
            }
        }
        for c in &spec.checks {
            if let Some(s) = c.spec.session() {
                if !ids.contains(s) {
                    return Err(Error::Invalid(format!("check {}: unknown session {s}", c.name)));
                }
            }
        }
        Ok(MemoryWorld {
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
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../eval/worlds/memory-crossday")
}

/// The recall-pressure world (for tests).
pub fn pressure_world_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../eval/worlds/memory-pressure")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_world_loads() {
        let w = MemoryWorld::load(&default_world_dir()).unwrap();
        assert!(MemoryWorld::is_memory_world(&w.dir));
        assert_eq!(w.spec.days.len(), 3);
        assert_eq!(w.spec.checks.len(), 8);
        assert!(w.spec.night_extract);
        assert!(!MemoryWorld::is_memory_world(
            &default_world_dir().join("../workspace-basic")
        ));
    }

    #[test]
    fn pressure_world_fills_the_profile_and_names_its_targets() {
        let w = MemoryWorld::load(&pressure_world_dir()).unwrap();
        let tagged = w.spec.seed_claims.iter().filter(|c| !c.tags.is_empty()).count();
        assert!(tagged > 12, "the fixed profile must overflow: {tagged} tagged");
        assert!(w.spec.seed_claims.len() >= 30);
        assert_eq!(w.spec.targets.len(), 4);
        assert!(!w.spec.night_extract);
    }
}
