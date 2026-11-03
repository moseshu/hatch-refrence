//! Identifier helpers.
//!
//! Stable ids are content-addressed (SHA-256 of the parts), which is what makes
//! re-running a workflow idempotent: the same mail always maps to the same id and
//! the same approval always maps to the same idempotency key.

use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Full hex SHA-256 digest of the parts, separated by an ASCII unit separator.
pub fn digest(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            h.update([0x1f]);
        }
        h.update(p.as_bytes());
    }
    hex::encode(h.finalize())
}

/// Deterministic id: `prefix_<16 hex chars>`.
pub fn stable_id(prefix: &str, parts: &[&str]) -> String {
    format!("{prefix}_{}", &digest(parts)[..16])
}

/// Unique id for things that have no natural key (sessions, runs, grants).
pub fn random_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let seed = format!("{nanos}:{n}:{}", std::process::id());
    format!("{prefix}_{}", &digest(&[&seed])[..16])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_ids_are_deterministic_and_separator_aware() {
        assert_eq!(stable_id("m", &["a", "b"]), stable_id("m", &["a", "b"]));
        assert_ne!(stable_id("m", &["ab", ""]), stable_id("m", &["a", "b"]));
        assert!(stable_id("m", &["x"]).starts_with("m_"));
    }

    #[test]
    fn random_ids_differ() {
        assert_ne!(random_id("r"), random_id("r"));
    }
}
