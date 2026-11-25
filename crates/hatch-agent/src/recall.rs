//! Memory context shared by the turn loop and process sub-agents.

use crate::config::RecallMode;
use hatch_core::time::Timestamp;
use hatch_memory::{Claim, MemoryStore};

const PROFILE_LIMIT: usize = 12;
const RECALL_LIMIT: usize = 5;

/// The fixed profile lines and the recalled lines for one turn.
pub(crate) fn memory_context(
    mem: &MemoryStore,
    query: &str,
    now: &Timestamp,
    mode: RecallMode,
) -> (Vec<String>, Vec<String>) {
    let mut claims = mem
        .with_tags(&["preference", "profile", "style"], now)
        .unwrap_or_else(|error| {
            mem.audit("profile_claims_failed", &error.to_string());
            Vec::new()
        });
    let profile = claims
        .iter()
        .take(PROFILE_LIMIT)
        .map(|c| format!("[{}] {}", c.id, c.text))
        .collect();
    let recalled = match mode {
        RecallMode::Off => Vec::new(),
        RecallMode::Auto => {
            claims.truncate(PROFILE_LIMIT);
            recall(mem, query, &claims, RECALL_LIMIT + claims.len(), now)
        }
        // Pre-0fe6079: all tagged claims excluded, top 5 taken first.
        RecallMode::LegacyExcludeAllTagged => recall(mem, query, &claims, RECALL_LIMIT, now),
    };
    (profile, recalled)
}

/// Recall claims relevant to this turn (BM25 keyword search, k = 5).
/// Never fails: a memory error means "no recall", never a failed turn.
/// No hits means no lines at all — the caller renders nothing.
///
/// P2: claims that actually enter the context are marked used
/// (`last_used_at`), so later turns rank them as recently used. A plain
/// search never marks anything — searching is not using.
///
/// `fetch` is how many ranked hits to read before dropping `exclude`;
/// each excluded claim can consume at most one ranked slot.
fn recall(mem: &MemoryStore, query: &str, exclude: &[Claim], fetch: usize, now: &Timestamp) -> Vec<String> {
    let hits = match mem.search(query, fetch, now) {
        Ok(hits) => hits,
        Err(e) => {
            // Degrade to "no recall" as before, but leave a trace.
            mem.audit("recall_failed", &e.to_string());
            return Vec::new();
        }
    };
    let hits: Vec<(Claim, f64)> = hits
        .into_iter()
        .filter(|(c, _)| !exclude.iter().any(|p| p.id == c.id))
        .take(RECALL_LIMIT)
        .collect();
    let ids: Vec<String> = hits.iter().map(|(c, _)| c.id.clone()).collect();
    let _ = mem.mark_used(&ids, now);
    hits.into_iter()
        .map(|(c, _)| {
            let tag = if c.trust.is_untrusted() {
                "（来源不可信）"
            } else {
                ""
            };
            format!(
                "[{}] {}（来源 {}，置信度 {:.2}，记于 {}）{tag}",
                c.id,
                c.text,
                c.source,
                c.confidence,
                c.created_at.format("%Y-%m-%d")
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use hatch_core::time::parse_rfc3339;
    use hatch_memory::{tokenize, NewClaim};
    use hatch_store::Db;

    #[test]
    fn matching_preference_beyond_the_profile_limit_is_recalled() {
        let db = Db::open_in_memory().unwrap();
        let mem = MemoryStore::new(&db);
        let now = parse_rfc3339("2026-11-25T00:00:00Z").unwrap();
        for minute in 1..=12 {
            let claim = NewClaim::stated(&format!("setting:{minute}"), "Use dark mode", "test")
                .with_tags(&["preference"]);
            mem.add(&claim, &(now - Duration::minutes(minute))).unwrap();
        }
        let target = NewClaim::stated("mail:signature", "Sign supplier replies as Alex", "test")
            .with_tags(&["style"]);
        let id = mem.add(&target, &now).unwrap().id().to_string();

        let (profile, recalled) = memory_context(&mem, "supplier", &now, RecallMode::Auto);
        assert_eq!(profile.len(), 12);
        assert!(profile
            .iter()
            .all(|line| !line.starts_with(&format!("[{id}]"))));
        assert_eq!(recalled.len(), 1);
        assert!(recalled[0].starts_with(&format!("[{id}]")));
    }

    #[test]
    fn injected_matches_do_not_use_recall_slots_or_get_marked_as_recalled() {
        let db = Db::open_in_memory().unwrap();
        let mem = MemoryStore::new(&db);
        let now = parse_rfc3339("2026-11-25T00:00:00Z").unwrap();
        let injected = NewClaim::stated("setting:reply", "Use the supplier template", "test")
            .with_tags(&["preference"])
            .with_salience(1.0);
        let injected_id = mem.add(&injected, &now).unwrap().id().to_string();
        let mut other_ids = Vec::new();
        for number in 0..6 {
            let claim = NewClaim::stated(
                &format!("mail:template{number}"),
                "Use the supplier template",
                "test",
            )
            .with_salience(0.1);
            other_ids.push(mem.add(&claim, &now).unwrap().id().to_string());
        }
        assert_eq!(
            mem.search("supplier", 5, &now).unwrap()[0].0.id,
            injected_id
        );

        let (profile, recalled) = memory_context(&mem, "supplier", &now, RecallMode::Auto);
        assert_eq!(profile.len(), 1);
        assert!(profile[0].starts_with(&format!("[{injected_id}]")));
        assert_eq!(recalled.len(), 5);
        assert!(recalled
            .iter()
            .all(|line| !line.starts_with(&format!("[{injected_id}]"))));
        assert_eq!(mem.get(&injected_id).unwrap().unwrap().last_used_at, None);
        for id in other_ids {
            let was_recalled = recalled
                .iter()
                .any(|line| line.starts_with(&format!("[{id}]")));
            assert_eq!(
                mem.get(&id).unwrap().unwrap().last_used_at,
                was_recalled.then_some(now)
            );
        }
    }

    #[test]
    fn preference_beyond_the_profile_limit_needs_an_indexed_term_match() {
        let db = Db::open_in_memory().unwrap();
        let mem = MemoryStore::new(&db);
        let now = parse_rfc3339("2026-11-25T00:00:00Z").unwrap();
        for minute in 1..=12 {
            let claim = NewClaim::stated(&format!("setting:{minute}"), "Use dark mode", "test")
                .with_tags(&["preference"]);
            mem.add(&claim, &(now - Duration::minutes(minute))).unwrap();
        }
        let target = NewClaim::stated("diet:restriction", "Avoid peanuts", "test")
            .with_tags(&["preference"]);
        let query = "午餐选哪家";
        let query_terms = tokenize(query);
        let indexed_terms = std::iter::once(target.subject.as_str())
            .chain(std::iter::once(target.text.as_str()))
            .chain(target.tags.iter().map(String::as_str))
            .flat_map(tokenize);
        assert!(!query_terms.is_empty());
        assert!(
            indexed_terms
                .into_iter()
                .all(|term| !query_terms.contains(&term)),
            "fixture must have no shared index terms"
        );
        let id = mem.add(&target, &now).unwrap().id().to_string();

        let (profile, recalled) = memory_context(&mem, query, &now, RecallMode::Auto);
        assert_eq!(profile.len(), 12);
        assert!(profile
            .iter()
            .all(|line| !line.starts_with(&format!("[{id}]"))));
        assert!(recalled.is_empty());
    }

    /// Twelve old tagged claims fill the profile; a newer tagged claim
    /// about suppliers sits beyond it.
    fn profile_plus_supplier_target(mem: &MemoryStore, now: &Timestamp) -> String {
        for minute in 1..=12 {
            let claim = NewClaim::stated(&format!("setting:{minute}"), "Use dark mode", "test")
                .with_tags(&["preference"]);
            mem.add(&claim, &(*now - Duration::minutes(minute))).unwrap();
        }
        let target = NewClaim::stated("mail:signature", "Sign supplier replies as Alex", "test")
            .with_tags(&["style"]);
        mem.add(&target, now).unwrap().id().to_string()
    }

    #[test]
    fn off_mode_keeps_the_profile_and_recalls_nothing() {
        let db = Db::open_in_memory().unwrap();
        let mem = MemoryStore::new(&db);
        let now = parse_rfc3339("2026-11-25T00:00:00Z").unwrap();
        let id = profile_plus_supplier_target(&mem, &now);

        let (profile, recalled) = memory_context(&mem, "supplier", &now, RecallMode::Off);
        assert_eq!(profile.len(), 12);
        assert!(recalled.is_empty());
        assert_eq!(mem.get(&id).unwrap().unwrap().last_used_at, None);
    }

    #[test]
    fn legacy_mode_reproduces_the_pre_fix_exclusion() {
        let db = Db::open_in_memory().unwrap();
        let mem = MemoryStore::new(&db);
        let now = parse_rfc3339("2026-11-25T00:00:00Z").unwrap();
        profile_plus_supplier_target(&mem, &now);
        let (profile, recalled) =
            memory_context(&mem, "supplier", &now, RecallMode::LegacyExcludeAllTagged);
        assert_eq!(profile.len(), 12);
        assert!(recalled.is_empty(), "tagged claims beyond the profile were never recalled");

        // A tagged top hit used to cost a slot without a refill.
        let db = Db::open_in_memory().unwrap();
        let mem = MemoryStore::new(&db);
        let injected = NewClaim::stated("setting:reply", "Use the supplier template", "test")
            .with_tags(&["preference"])
            .with_salience(1.0);
        mem.add(&injected, &now).unwrap();
        for number in 0..6 {
            let claim = NewClaim::stated(
                &format!("mail:template{number}"),
                "Use the supplier template",
                "test",
            )
            .with_salience(0.1);
            mem.add(&claim, &now).unwrap();
        }
        let (_, legacy) =
            memory_context(&mem, "supplier", &now, RecallMode::LegacyExcludeAllTagged);
        assert_eq!(legacy.len(), 4);
    }
}
