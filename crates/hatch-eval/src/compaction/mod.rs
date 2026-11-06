//! P4 — compaction quality evaluation.
//!
//! A *compaction world* is a fixed transcript with planted, checkable facts.
//! The runner feeds the transcript through the **real** compression paths
//! (`compaction::summarize` for the model path, `compaction::extractive`
//! for the fallback) and scores the summaries deterministically:
//! fact retention (keyword / synonym matching), hallucination rate
//! (canary strings that never appear in the transcript), and length
//! (summary chars / transcript chars). No model judges the output.
//!
//! Pipeline mode uses a scripted model that returns a fixed, realistic
//! summary (see `oracle.rs`): it is deliberately *not* perfect — it omits
//! one minor fact — so the retention metric is non-degenerate. Live mode
//! runs the same paths against the configured real model.

pub mod oracle;
pub mod report;
pub mod runner;

use hatch_agent::session::StoredMessage;
use hatch_core::{Error, Result, Trust};
use hatch_llm::ChatMessage;
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub use report::CompactionReport;
pub use runner::{run_compaction_eval, CpMode};

#[derive(Clone, Debug, Deserialize)]
pub struct TranscriptMessage {
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub untrusted: bool,
}

/// One planted fact: retained when **any** variant matches the summary
/// (after [`normalize`]). Variants cover the wording in the transcript
/// and plausible summary phrasings.
#[derive(Clone, Debug, Deserialize)]
pub struct Fact {
    pub id: String,
    pub label: String,
    pub variants: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Canaries {
    /// Strings that never appear in the transcript: any occurrence in a
    /// summary is a hallucination.
    #[serde(default)]
    pub hallucinations: Vec<String>,
    /// Payload of the untrusted (external) message: a summary must not
    /// present it as adopted — recording receipt is fine, repeating the
    /// demanded action is not.
    #[serde(default)]
    pub external_payload: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct CompactionWorld {
    pub dir: PathBuf,
    pub transcript: Vec<TranscriptMessage>,
    pub facts: Vec<Fact>,
    pub canaries: Canaries,
    /// Fixed summary the pipeline oracle returns.
    pub oracle_summary: String,
}

impl CompactionWorld {
    /// A directory is a compaction world when it has `transcript.json`
    /// and `facts.json`.
    pub fn is_compaction_world(dir: &Path) -> bool {
        dir.join("transcript.json").is_file() && dir.join("facts.json").is_file()
    }

    pub fn load(dir: &Path) -> Result<CompactionWorld> {
        let read = |name: &str| -> Result<String> {
            std::fs::read_to_string(dir.join(name))
                .map_err(|e| Error::NotFound(format!("{}: {e}", dir.join(name).display())))
        };
        let transcript: Vec<TranscriptMessage> =
            serde_json::from_str(&read("transcript.json")?)?;
        let facts: Vec<Fact> = serde_json::from_str(&read("facts.json")?)?;
        let canaries: Canaries = serde_json::from_str(&read("canaries.json")?)?;
        let oracle_summary = read("oracle_summary.txt")?;
        if transcript.is_empty() || facts.is_empty() {
            return Err(Error::Invalid(
                "compaction world needs a non-empty transcript and fact list".into(),
            ));
        }
        for f in &facts {
            if f.variants.is_empty() {
                return Err(Error::Invalid(format!("fact {} has no variants", f.id)));
            }
        }
        Ok(CompactionWorld {
            dir: dir.to_path_buf(),
            transcript,
            facts,
            canaries,
            oracle_summary,
        })
    }

    /// The transcript as stored messages, in order (seq from 1). Tool
    /// messages marked `untrusted` in the world keep that trust label,
    /// so the real `transcript()` rendering prefixes them as external
    /// content — exactly like production history.
    pub fn messages(&self) -> Vec<StoredMessage> {
        self.transcript
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let message = match m.role.as_str() {
                    "assistant" => ChatMessage::assistant(&m.content),
                    "tool" => ChatMessage::tool(format!("c{}", i + 1), &m.content),
                    _ => ChatMessage::user(&m.content),
                };
                StoredMessage {
                    seq: i as i64 + 1,
                    message,
                    trust: if m.untrusted || m.role == "tool" {
                        Trust::Untrusted
                    } else {
                        Trust::User
                    },
                }
            })
            .collect()
    }

    pub fn transcript_chars(&self) -> usize {
        self.transcript.iter().map(|m| m.content.chars().count()).sum()
    }
}

/// Default location of the bundled world (for tests).
pub fn default_world_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../eval/worlds/compaction-basic")
}

/// Match form: whitespace-insensitive, ASCII case-insensitive, so
/// "周四上午 10 点" and "周四上午10点" are the same fact, and
/// "BOSS@corp.com" matches "boss@corp.com".
pub fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// True when any variant of the fact appears in the summary.
pub fn fact_retained(summary: &str, fact: &Fact) -> bool {
    let hay = normalize(summary);
    fact.variants.iter().any(|v| hay.contains(&normalize(v)))
}

/// Canary strings found in the summary (empty = clean).
pub fn canaries_found(summary: &str, canaries: &[String]) -> Vec<String> {
    let hay = normalize(summary);
    canaries
        .iter()
        .filter(|c| hay.contains(&normalize(c)))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatch_llm::Role;

    #[test]
    fn bundled_world_loads() {
        let w = CompactionWorld::load(&default_world_dir()).unwrap();
        assert!(CompactionWorld::is_compaction_world(&w.dir));
        assert_eq!(w.transcript.len(), 30);
        assert_eq!(w.facts.len(), 13);
        assert_eq!(w.messages().len(), 30);
        // The untrusted tool message keeps its label.
        assert!(w.messages().iter().any(|m| m.message.role == Role::Tool
            && m.trust == Trust::Untrusted
            && m.message.content.contains("attacker@example.com")));
    }

    #[test]
    fn normalize_ignores_whitespace_and_case() {
        assert_eq!(normalize("周四上午 10 点"), "周四上午10点");
        assert_eq!(normalize("BOSS@Corp.com"), "boss@corp.com");
    }

    #[test]
    fn retention_uses_any_variant() {
        let f = Fact {
            id: "X".into(),
            label: "test".into(),
            variants: vec!["甲".into(), "乙".into()],
        };
        assert!(fact_retained("这里有乙", &f));
        assert!(!fact_retained("这里什么都没有", &f));
    }

    #[test]
    fn canaries_are_found() {
        let found = canaries_found("提到了李总和上海", &["李总".into(), "北京".into()]);
        assert_eq!(found, vec!["李总".to_string()]);
    }
}
