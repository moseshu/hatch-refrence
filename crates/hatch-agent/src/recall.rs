//! Memory context shared by the turn loop and process sub-agents.

use hatch_core::time::Timestamp;
use hatch_memory::{Claim, MemoryStore};

const PROFILE_LIMIT: usize = 12;
const RECALL_LIMIT: usize = 5;

pub(crate) fn memory_context(
    mem: &MemoryStore,
    query: &str,
    now: &Timestamp,
) -> (Vec<String>, Vec<String>) {
    let mut claims = mem
        .with_tags(&["preference", "profile", "style"], now)
        .unwrap_or_else(|error| {
            mem.audit("profile_claims_failed", &error.to_string());
            Vec::new()
        });
    claims.truncate(PROFILE_LIMIT);
    let profile = claims
        .iter()
        .map(|c| format!("[{}] {}", c.id, c.text))
        .collect();
    let recalled = recall(mem, query, &claims, now);
    (profile, recalled)
}

/// Recall claims relevant to this turn (BM25 keyword search, k = 5).
/// Never fails: a memory error means "no recall", never a failed turn.
/// No hits means no lines at all — the caller renders nothing.
///
/// P2: claims that actually enter the context are marked used
/// (`last_used_at`), so later turns rank them as recently used. A plain
/// search never marks anything — searching is not using.
fn recall(mem: &MemoryStore, query: &str, exclude: &[Claim], now: &Timestamp) -> Vec<String> {
    // Each injected profile can consume at most one ranked slot.
    let hits = match mem.search(query, RECALL_LIMIT + exclude.len(), now) {
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

        let (profile, recalled) = memory_context(&mem, "supplier", &now);
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

        let (profile, recalled) = memory_context(&mem, "supplier", &now);
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

        let (profile, recalled) = memory_context(&mem, query, &now);
        assert_eq!(profile.len(), 12);
        assert!(profile
            .iter()
            .all(|line| !line.starts_with(&format!("[{id}]"))));
        assert!(recalled.is_empty());
    }
}
