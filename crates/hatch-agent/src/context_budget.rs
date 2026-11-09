//! §3.4 context engineering: the compaction trigger is a total-context
//! budget, not a fixed absolute history size.
//!
//! ```text
//! prefix + history > effective_budget  →  compact
//! θ = effective_budget − prefix         (history threshold, recomputed
//!                                        every turn from the measured prefix)
//! effective_budget = min(total_budget, window × ratio)
//! ```
//!
//! * `total_budget` is a quality/cost choice, not a window. Default
//!   64k — a starting proposal to be validated by experiment (32k /
//!   64k / 128k), not a settled number.
//! * The window comes from the model's capability declaration (§3.4,
//!   OMB #23): static configuration, never hardcoded per model, never
//!   a provider usage reading. Unknown window → the absolute term only.
//! * The relative term (`window × ratio`, default 0.25) rules small
//!   windows; the total rules big ones — the `min` takes whichever
//!   binds first.
//! * The hard floor survives: history past `2 × θ` compacts
//!   synchronously with the extractive fallback (see `agent.rs`).
//! * Cooldown: after a compaction, new history must reach half the
//!   threshold before the next trigger, so the same stretch is never
//!   compacted twice in a row. Summaries are chained (a new summary
//!   folds the previous one in — see `compaction::summarize`).
//!
//! All token counts here are hatch's own chars/4 estimates. Provider
//! usage is a meter for display only and never feeds a compaction
//! decision.

/// Starting proposal for `total_budget` (§3.4): 64k, to be validated by
/// the 32k / 64k / 128k experiment. Configurable (`[agent]
/// total_budget`).
pub const DEFAULT_TOTAL_BUDGET: usize = 64_000;

/// Starting proposal for the window ratio (§3.4): a quarter of the
/// declared window. Configurable (`[agent] window_ratio`).
pub const DEFAULT_WINDOW_RATIO: f64 = 0.25;

/// The budget a turn's whole context must fit: the smaller of the
/// absolute total and the window-relative term. Unknown window → the
/// absolute term only. An out-of-range ratio (0, 1] is a config error
/// caught at load time; defensively, fall back to the default ratio.
pub fn effective_budget(
    total_budget: usize,
    context_window: Option<usize>,
    window_ratio: f64,
) -> usize {
    let ratio = if window_ratio > 0.0 && window_ratio <= 1.0 {
        window_ratio
    } else {
        DEFAULT_WINDOW_RATIO
    };
    match context_window {
        Some(w) => total_budget.min(((w as f64) * ratio) as usize),
        None => total_budget,
    }
}

/// History threshold: how much live history the turn can still carry
/// once the measured stable prefix is paid for. Saturates at 0 — a
/// prefix at or over budget compacts at once.
pub fn history_threshold(effective_budget: usize, prefix_tokens: usize) -> usize {
    effective_budget.saturating_sub(prefix_tokens)
}

/// The trigger itself: compact when prefix + history exceeds the
/// effective budget. Written via the threshold so the formula has one
/// source of truth.
pub fn should_compact(prefix_tokens: usize, history_tokens: usize, effective_budget: usize) -> bool {
    history_tokens > history_threshold(effective_budget, prefix_tokens)
}

/// Cooldown: new history since the last compaction must reach half the
/// threshold before the next trigger. A zero threshold (prefix already
/// at budget) never waits — it must compact immediately.
pub fn cooldown_passed(new_history_tokens: usize, threshold: usize) -> bool {
    threshold == 0 || new_history_tokens.saturating_mul(2) >= threshold
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_boundary_is_strict() {
        // prefix + history == budget does not trigger; one token over does.
        assert!(!should_compact(600, 400, 1000));
        assert!(should_compact(600, 401, 1000));
        assert_eq!(history_threshold(1000, 600), 400);
    }

    #[test]
    fn unknown_window_falls_back_to_total_only() {
        assert_eq!(effective_budget(64_000, None, 0.25), 64_000);
    }

    #[test]
    fn small_window_is_ruled_by_the_ratio() {
        // 8k window × 0.25 = 2k binds before the 64k total.
        assert_eq!(effective_budget(64_000, Some(8_000), 0.25), 2_000);
        // 1M window × 0.25 = 250k: the total rules.
        assert_eq!(effective_budget(64_000, Some(1_000_000), 0.25), 64_000);
    }

    #[test]
    fn prefix_over_budget_compacts_at_once() {
        assert_eq!(history_threshold(1000, 1500), 0);
        assert!(should_compact(1500, 1, 1000));
        // …and the cooldown never blocks a zero threshold.
        assert!(cooldown_passed(0, 0));
    }

    #[test]
    fn cooldown_needs_half_the_threshold_of_new_history() {
        assert!(!cooldown_passed(499, 1000));
        assert!(cooldown_passed(500, 1000), "exactly half passes");
        assert!(cooldown_passed(1000, 1000));
    }

    #[test]
    fn out_of_range_ratio_falls_back_to_default() {
        // 0 or >1 is a config error (validated at load); the pure
        // function must not produce a degenerate 0 budget from it.
        assert_eq!(effective_budget(64_000, Some(8_000), 0.0), 2_000);
        assert_eq!(effective_budget(64_000, Some(8_000), 2.0), 2_000);
    }
}
