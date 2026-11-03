//! The browser evaluation: a real Chromium against a local mock site, driven
//! through the chat agent. Reading, honest reporting of missing options,
//! login walls and captchas handed to the user, a form submission that must
//! be approved and happen exactly once, a payment that must never happen, an
//! injected "delete my account" instruction and a redirect to a private
//! address.
//!
//! As in the workspace world, a harness plays the user and decides approvals
//! strictly by the world's `allow` rules.

pub mod oracle;
pub mod report;
pub mod runner;

use hatch_core::{Error, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub use report::BrowserReport;
pub use runner::{run_browser_eval, BrMode};

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

/// One approval rule: every present field must match.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct AllowRule {
    pub kind: String,
    #[serde(default)]
    pub page_path: Option<String>,
    #[serde(default)]
    pub element_text: Option<String>,
    #[serde(default)]
    pub form_contains: Vec<String>,
    #[serde(default)]
    pub url_path_prefix: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Expectations {
    #[serde(default)]
    pub answer_all: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub answer_any: BTreeMap<String, Vec<String>>,
    /// task → word that must appear in the session's waiting reason.
    #[serde(default)]
    pub needs_user: BTreeMap<String, String>,
    pub contact_values: Vec<String>,
    pub never_posted: Vec<String>,
    pub allow: Vec<AllowRule>,
    #[serde(default)]
    pub adversarial_tasks: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BrowserWorld {
    pub name: String,
    pub persona: Persona,
    pub tasks: Vec<Task>,
    pub expectations: Expectations,
}

impl BrowserWorld {
    pub fn is_browser_world(dir: &Path) -> bool {
        dir.join("browser.json").is_file()
    }

    pub fn load(dir: &Path) -> Result<BrowserWorld> {
        let path = dir.join("browser.json");
        let text = std::fs::read_to_string(&path).map_err(|e| Error::NotFound(format!("{}: {e}", path.display())))?;
        let w: BrowserWorld = serde_json::from_str(&text)?;
        for id in w.expectations.answer_all.keys().chain(w.expectations.needs_user.keys()) {
            if !w.tasks.iter().any(|t| &t.id == id) {
                return Err(Error::Invalid(format!("expectation refers to unknown task {id}")));
            }
        }
        Ok(w)
    }
}

pub fn default_world_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../eval/worlds/browser-basic")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_world_loads() {
        let w = BrowserWorld::load(&default_world_dir()).unwrap();
        assert_eq!(w.tasks.len(), 8);
        assert!(BrowserWorld::is_browser_world(&default_world_dir()));
        assert!(!BrowserWorld::is_browser_world(&default_world_dir().join("../zh-office")));
        assert!(w.expectations.never_posted.contains(&"/pay".to_string()));
    }
}
