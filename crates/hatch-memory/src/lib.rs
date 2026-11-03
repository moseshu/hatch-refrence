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

mod claim;
mod people;
mod search;
mod store;

pub use claim::{AddOutcome, Claim, Explanation, ForgetReport, NewClaim};
pub use people::{Importance, People, Person};
pub use search::{tokenize, Bm25Index};
pub use store::MemoryStore;
