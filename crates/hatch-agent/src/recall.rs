//! Memory recall for a turn (BM25 keyword search, k = 5).
//!
//! Split out of [`crate::agent`] to keep that file under the line limit:
//! the turn loop only calls [`recall`] from [`crate::agent::Agent::layers`].

use hatch_core::time::Timestamp;
use hatch_memory::{Claim, MemoryStore};

/// Recall claims relevant to this turn (BM25 keyword search, k = 5).
/// Never fails: a memory error means "no recall", never a failed turn.
/// No hits means no lines at all — the caller renders nothing.
///
/// P2: claims that actually enter the context are marked used
/// (`last_used_at`), so later turns rank them as recently used. A plain
/// search never marks anything — searching is not using.
pub(crate) fn recall(mem: &MemoryStore, query: &str, exclude: &[Claim], now: &Timestamp) -> Vec<String> {
    let Ok(hits) = mem.search(query, 5, now) else {
        return Vec::new();
    };
    let hits: Vec<(Claim, f64)> = hits
        .into_iter()
        .filter(|(c, _)| !exclude.iter().any(|p| p.id == c.id))
        .collect();
    let ids: Vec<String> = hits.iter().map(|(c, _)| c.id.clone()).collect();
    let _ = mem.mark_used(&ids, now);
    hits.into_iter()
        .map(|(c, _)| {
            let tag = if c.trust.is_untrusted() { "（来源不可信）" } else { "" };
            format!("[{}] {}（来源 {}，置信度 {:.2}）{tag}", c.id, c.text, c.source, c.confidence)
        })
        .collect()
}
