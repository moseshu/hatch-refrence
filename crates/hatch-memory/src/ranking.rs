//! P2 ranking: raw BM25 × salience × time decay.
//!
//! The formula is deliberately simple and explainable:
//!
//! * `salience` (0.0–1.0, default 0.5 = neutral) is the claim's own weight
//!   — a 0.9 claim scores 1.8× a 0.5 claim on the same BM25 hit, and a
//!   0.0 claim never surfaces;
//! * `decay` is exponential in the time since the claim was last used:
//!   1.0 when just used, 0.5 after one half-life, floored so a very old
//!   claim can still surface on a strong BM25 hit instead of vanishing.
//!
//! Both retrieval paths (persistent index and legacy in-memory build)
//! rescore with this one function in the store, so the oracle test still
//! compares identical scores.

use crate::claim::Claim;
use hatch_core::time::Timestamp;

/// Ranking knobs. The half-life is configurable via
/// `[agent] salience_half_life_days` (default 90 days).
#[derive(Clone, Copy, Debug)]
pub struct RankingConfig {
    pub half_life_days: f64,
    /// A claim never decays below this factor, however old.
    pub decay_floor: f64,
}

impl Default for RankingConfig {
    fn default() -> Self {
        RankingConfig {
            half_life_days: 90.0,
            decay_floor: 0.05,
        }
    }
}

impl RankingConfig {
    pub fn from_days(half_life_days: f64) -> Self {
        RankingConfig {
            half_life_days: half_life_days.max(1.0),
            ..Default::default()
        }
    }
}

/// Effective last use: explicit `last_used_at`, else creation time.
/// A claim never injected into a turn is treated as last used when it
/// was created — old and unused decays, fresh and unused does not.
pub fn effective_last_used(c: &Claim) -> Timestamp {
    c.last_used_at.unwrap_or(c.created_at)
}

/// Exponential decay in days since last use, floored at `decay_floor`.
pub fn decay_factor(c: &Claim, now: &Timestamp, cfg: RankingConfig) -> f64 {
    let age_days =
        (*now - effective_last_used(c)).num_seconds().max(0) as f64 / 86_400.0;
    cfg.decay_floor
        .max(0.5f64.powf(age_days / cfg.half_life_days))
}

/// Combined recall score: raw BM25 × salience × decay.
pub fn combined_score(bm25: f64, c: &Claim, now: &Timestamp, cfg: RankingConfig) -> f64 {
    bm25 * c.salience * decay_factor(c, now, cfg)
}

/// Rescore a candidate list with the combined score, best first,
/// zero-scored (salience 0) dropped, truncated to k. Shared by both
/// retrieval paths so the oracle test compares identical scores.
pub fn rescore(
    hits: Vec<(Claim, f64)>,
    k: usize,
    now: &Timestamp,
    cfg: RankingConfig,
) -> Vec<(Claim, f64)> {
    let mut out: Vec<(Claim, f64)> = hits
        .into_iter()
        .map(|(c, s)| (c.clone(), combined_score(s, &c, now, cfg)))
        .filter(|(_, s)| *s > 0.0)
        .collect();
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    out.truncate(k);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use hatch_core::time::now;
    use hatch_core::Trust;

    fn claim(salience: f64, last_used: Option<Timestamp>, created: Timestamp) -> Claim {
        Claim {
            id: "clm_x".into(),
            subject: "user".into(),
            text: "x".into(),
            tags: vec![],
            source: "user:chat".into(),
            trust: Trust::User,
            confidence: 0.9,
            created_at: created,
            valid_until: None,
            supersedes: None,
            replaced_by: None,
            inferred: false,
            salience,
            last_used_at: last_used,
        }
    }

    #[test]
    fn decay_halves_each_half_life_and_floors() {
        let now = now();
        let cfg = RankingConfig::default();
        let c = claim(0.5, Some(now), now);
        assert!((decay_factor(&c, &now, cfg) - 1.0).abs() < 1e-9);
        let c = claim(0.5, Some(now - Duration::days(90)), now);
        assert!((decay_factor(&c, &now, cfg) - 0.5).abs() < 1e-9);
        let c = claim(0.5, Some(now - Duration::days(180)), now);
        assert!((decay_factor(&c, &now, cfg) - 0.25).abs() < 1e-9);
        let c = claim(0.5, Some(now - Duration::days(900)), now);
        assert!((decay_factor(&c, &now, cfg) - 0.05).abs() < 1e-9, "floored");
    }

    #[test]
    fn null_last_used_falls_back_to_created_at() {
        let now = now();
        let cfg = RankingConfig::default();
        let fresh = claim(0.5, None, now);
        assert!((decay_factor(&fresh, &now, cfg) - 1.0).abs() < 1e-9);
        let old = claim(0.5, None, now - Duration::days(180));
        assert!((decay_factor(&old, &now, cfg) - 0.25).abs() < 1e-9);
    }

    #[test]
    fn salience_scales_score_and_zero_never_surfaces() {
        let now = now();
        let cfg = RankingConfig::default();
        let hi = claim(0.9, Some(now), now);
        let lo = claim(0.3, Some(now), now);
        assert!((combined_score(2.0, &hi, &now, cfg) - 1.8).abs() < 1e-9);
        assert!((combined_score(2.0, &lo, &now, cfg) - 0.6).abs() < 1e-9);
        let zero = claim(0.0, Some(now), now);
        assert_eq!(combined_score(2.0, &zero, &now, cfg), 0.0);
    }

    #[test]
    fn combined_never_exceeds_raw_bm25() {
        let now = now();
        let cfg = RankingConfig::default();
        let c = claim(0.9, Some(now - Duration::days(30)), now);
        assert!(combined_score(3.0, &c, &now, cfg) <= 3.0);
    }
}
