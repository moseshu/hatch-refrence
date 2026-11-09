//! §3.4 compaction trigger: when a turn compacts, and how.
//!
//! The threshold is a total-context budget, not a fixed absolute
//! history size: the caller (the turn loop) passes the measured
//! stable-prefix tokens, and the history threshold is
//! `θ = effective_budget − prefix`, recomputed every turn. A cooldown
//! applies after a compaction: new history since the last summary
//! must reach half the threshold before the next trigger, so the same
//! stretch is never compacted twice in a row.
//!
//! Lives apart from `agent.rs` so the turn-loop file stays under the
//! 400-line limit.

use crate::agent::Agent;
use crate::assemble::message_tokens;
use crate::compaction;
use crate::session::SessionStore;
use crate::worker;
use hatch_core::time::Timestamp;
use hatch_core::Result;

impl<'a> Agent<'a> {
    /// Decide what to do about a history over the compaction threshold.
    /// Returns `(compacted_now, compaction_requested)`:
    /// * over the hard limit (2× the history threshold) the turn
    ///   compacts synchronously with the extractive fallback — no model
    ///   call, so the turn can always continue;
    /// * otherwise a one-shot background job is enqueued (deduped per
    ///   session) and this turn continues with the full history. The
    ///   summary lands for the next turn.
    ///
    /// A bookkeeping failure while enqueueing never fails the turn:
    /// compaction is an optimisation, and the hard limit above still
    /// protects the context.
    pub(crate) fn maybe_compact(
        &self,
        sessions: &SessionStore,
        session: &str,
        now: &Timestamp,
        prefix_tokens: usize,
    ) -> Result<(bool, bool)> {
        let (summary, history) = sessions.active(session)?;
        let budget = self.config.effective_budget();
        let theta = crate::context_budget::history_threshold(budget, prefix_tokens);
        // Cooldown: only history appended after the last summary counts
        // as new. The kept tail predates the summary row, so it never
        // counts — otherwise the tail alone would re-trigger at once.
        if let Some(last) = sessions.last_summary_seq(session)? {
            let new_tokens: usize = history
                .iter()
                .filter(|m| m.seq > last)
                .map(|m| message_tokens(&m.message))
                .sum();
            if !crate::context_budget::cooldown_passed(new_tokens, theta) {
                return Ok((false, false));
            }
        }
        let Some(p) = compaction::plan(&history, theta, self.config.keep_tail_turns) else {
            return Ok((false, false));
        };
        let total: usize = history.iter().map(|m| message_tokens(&m.message)).sum();
        if total > theta.saturating_mul(2) {
            let text = compaction::extractive(summary.as_deref(), &history[..p.count]);
            sessions.compact(session, p.upto_seq, &text, now)?;
            return Ok((true, false));
        }
        let requested =
            worker::request_compaction(self.db, session, p.upto_seq, now, &self.config.tz)
                .unwrap_or(false);
        Ok((false, requested))
    }
}
