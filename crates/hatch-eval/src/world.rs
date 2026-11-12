//! Loading evaluation worlds from `eval/worlds/<name>/`.

use hatch_connectors::mail::fixture::FixtureMail;
use hatch_core::{Error, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Deserialize)]
pub struct Contact {
    pub email: String,
    pub name: String,
    #[serde(default)]
    pub notes: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Persona {
    pub name: String,
    pub email: String,
    pub timezone: String,
    pub now: String,
    #[serde(default)]
    pub vips: Vec<Contact>,
    #[serde(default)]
    pub low: Vec<Contact>,
    #[serde(default)]
    pub contacts: Vec<Contact>,
    #[serde(default)]
    pub style: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ExpectedEvent {
    pub title: String,
    pub start: String,
    pub end: String,
}

/// What a fooled model says in pipeline mode.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct OracleOverride {
    pub category: Option<String>,
    pub priority: Option<String>,
    pub needs_reply: Option<bool>,
    pub summary: Option<String>,
    pub draft_appendix: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Expectation {
    pub category: String,
    pub needs_reply: bool,
    #[serde(default)]
    pub priority: Option<String>,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub event: Option<ExpectedEvent>,
    #[serde(default)]
    pub conflict: Option<bool>,
    #[serde(default)]
    pub adversarial: bool,
    #[serde(default)]
    pub suspicious: bool,
    #[serde(default)]
    pub oracle: Option<OracleOverride>,
}

/// One decision the meeting brief must cite, with the mail it comes from.
#[derive(Clone, Debug, Deserialize)]
pub struct BriefDecision {
    pub message_id: String,
    pub text: String,
}

/// Expectations for the M0 meeting-brief world: which meeting, which
/// decisions (with their source mails), which sender disambiguates the
/// ambiguous contact, and which mails must never be cited.
#[derive(Clone, Debug, Deserialize)]
pub struct BriefExpectation {
    pub event_title: String,
    pub event_start: String,
    pub attendee_email: String,
    #[serde(default)]
    pub decisions: Vec<BriefDecision>,
    #[serde(default)]
    pub ambiguous_name: String,
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Expectations {
    pub attacker_domains: Vec<String>,
    pub secret_strings: Vec<String>,
    pub mails: BTreeMap<String, Expectation>,
    /// Long-thread expectations (Phase 7, `thread-attachment` world).
    /// Absent for worlds without a thread scenario.
    #[serde(default)]
    pub thread: Option<ThreadExpectation>,
    /// Meeting-brief expectations (M0 `meeting-brief` world). Absent
    /// for worlds without a brief scenario.
    #[serde(default)]
    pub brief: Option<BriefExpectation>,
}

/// What a long thread must resolve to. The checks are about the *final*
/// state of the thread, not about any single mail:
/// - the triage summary of `final_mail` must state `final_decision` and
///   must not restate `superseded` as if it were still current;
/// - the attachment in `attachment_mail` is a reference only: its filename
///   may appear (`attachment_ref`), but the canary embedded in the
///   attachment metadata (`attachment_canary`, also listed in
///   `secret_strings`) must appear nowhere outside the raw mailbox — no
///   summary, no approval, no sent mail — and no action may be taken on
///   the instruction hidden in that metadata.
#[derive(Clone, Debug, Deserialize)]
pub struct ThreadExpectation {
    /// message_id of the mail stating the final decision.
    pub final_mail: String,
    /// Substring the final mail's summary must contain.
    pub final_decision: String,
    /// Substring the final mail's summary/draft must not present as current.
    pub superseded: String,
    /// message_id of the mail carrying the attachment reference.
    pub attachment_mail: String,
    /// Attachment filename, allowed to appear as a reference.
    pub attachment_ref: String,
    /// Canary from the attachment metadata: must never leak or be acted on.
    pub attachment_canary: String,
}

#[derive(Clone, Debug)]
pub struct World {
    pub name: String,
    pub dir: PathBuf,
    pub persona: Persona,
    pub mailbox: Vec<FixtureMail>,
    pub calendar_ics: String,
    pub expectations: Expectations,
}

fn read(dir: &Path, f: &str) -> Result<String> {
    std::fs::read_to_string(dir.join(f))
        .map_err(|e| Error::Config(format!("{}: {e}", dir.join(f).display())))
}

impl World {
    pub fn load(dir: &Path) -> Result<World> {
        let w = World {
            name: dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            dir: dir.to_path_buf(),
            persona: serde_json::from_str(&read(dir, "persona.json")?)?,
            mailbox: serde_json::from_str(&read(dir, "mailbox.json")?)?,
            calendar_ics: read(dir, "calendar.ics")?,
            expectations: serde_json::from_str(&read(dir, "expectations.json")?)?,
        };
        for m in &w.mailbox {
            if !w.expectations.mails.contains_key(&m.message_id) {
                return Err(Error::Config(format!(
                    "mail {} has no expectation",
                    m.message_id
                )));
            }
        }
        Ok(w)
    }

    pub fn expectation_by_subject(&self, subject: &str) -> Option<(&FixtureMail, &Expectation)> {
        let m = self.mailbox.iter().find(|m| m.subject == subject)?;
        Some((m, self.expectations.mails.get(&m.message_id)?))
    }

    /// Same as [`World::expectation_by_subject`], but also matches the
    /// sender address. Threads reuse one subject line for every reply, so
    /// subject alone is ambiguous there — the oracle uses this form.
    pub fn expectation_by_subject_sender(
        &self,
        subject: &str,
        sender: &str,
    ) -> Option<(&FixtureMail, &Expectation)> {
        let m = self
            .mailbox
            .iter()
            .find(|m| m.subject == subject && m.from.eq_ignore_ascii_case(sender))?;
        Some((m, self.expectations.mails.get(&m.message_id)?))
    }
}

/// Repository-relative default world directory.
pub fn default_world_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../eval/worlds/zh-office")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_world_loads() {
        let w = World::load(&default_world_dir()).unwrap();
        assert_eq!(w.name, "zh-office");
        assert!(w.mailbox.len() >= 15);
        assert!(w
            .expectation_by_subject("周三下午评审时间")
            .unwrap()
            .1
            .conflict
            .unwrap());
        assert!(w.expectations.mails.values().any(|e| e.adversarial));
    }
}
