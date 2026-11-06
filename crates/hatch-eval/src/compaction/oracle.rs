//! Scripted model for the compaction world's pipeline mode.
//!
//! It returns the world's fixed `oracle_summary.txt` for every request, so
//! pipeline scores are deterministic. The fixed summary is deliberately a
//! *realistic* summary, not a perfect one: it retains 12 of the 13 planted
//! facts (it omits the ¥800 reimbursement cap buried late in a long
//! message) and contains no canary. That makes the retention metric
//! non-degenerate — a perfect oracle would make the check untestable —
//! while still passing the ≥ 80% bar a summary needs to be useful for
//! continuing the conversation.
//!
//! This is the opposite of the other worlds' "fooled model" convention:
//! those oracles deliberately fall for planted instructions to prove the
//! defences hold. Here the model is not under test — the *compression
//! paths* are. A fooled oracle would measure nothing about compaction
//! quality, so the oracle plays a competent summariser instead.

use hatch_llm::scripted::FnModel;
use hatch_llm::{ChatRequest, ChatResponse, LlmError, Usage};
use std::sync::Arc;

fn usage(req: &ChatRequest) -> Usage {
    let prompt: usize = req
        .messages
        .iter()
        .map(|m| hatch_core::tokens::estimate_tokens(&m.content))
        .sum();
    Usage {
        prompt_tokens: prompt as u64,
        completion_tokens: 40,
        ..Default::default()
    }
}

pub fn compaction_oracle(summary: String) -> Arc<FnModel> {
    Arc::new(FnModel::new("compaction-oracle", move |req: &ChatRequest| {
        let mut r = ChatResponse::text(summary.clone());
        r.usage = usage(req);
        Ok::<ChatResponse, LlmError>(r)
    }))
}
