//! `hatch-memory` — what the assistant knows about the user and their world.
//!
//! Memory is a set of **claims**, not free text. Each claim has a subject, a
//! source, a trust label, a confidence and an optional expiry. Corrections never
//! overwrite: they append a new claim and link old ↔ new, so `explain` can show
//! where a fact came from and what replaced it. Write discipline is enforced
//! here, not in prompts:
//!
//! * secrets (passwords, OTPs, card/id numbers, keys) are rejected outright;
//! * inferred claims are capped at 0.7 confidence, untrusted sources at 0.5;
//! * duplicates of a current claim are folded into the existing one.
//!
//! Memory never grants permission — it is data for the policy layer, not input to it.
//!
//! Storage is split in two: the **bank** (`bank.rs`) owns the claims, and
//! the **index** (`index.rs`) owns the persistent BM25 retrieval structure.
//! Callers only see the `MemoryStore` facade; bank writes update the index
//! in the same transaction, and the index self-heals (verify/rebuild) when
//! it drifts.
//!
//! P2 ranking (`ranking.rs`): search scores are raw BM25 × salience
//! (0.0–1.0, default 0.5) × exponential time decay on `last_used_at`
//! (half-life 90 days by default, floored at 0.05). Claims are marked
//! used only when injected into a turn's context — never by a plain search.

mod bank;
mod claim;
mod index;
mod people;
mod ranking;
mod search;
mod store;

pub use claim::{AddOutcome, Claim, Explanation, ForgetReport, NewClaim};
pub use index::{IndexReport, MemoryIndex};
pub use people::{Importance, People, Person};
pub use ranking::{combined_score, decay_factor, effective_last_used, RankingConfig};
pub use search::{tokenize, Bm25Index};
pub use store::MemoryStore;
