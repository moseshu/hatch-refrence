//! Task 9: release management and continuous improvement.
//!
//! The pieces, each small enough to audit on its own:
//!
//! * [`attribution`] — a failure is one of six classes (runtime,
//!   model, tool, context, permission, delivery) at one pipeline
//!   stage, so a regression can be located instead of guessed at.
//! * [`sample`] — desensitized failure traces kept as eval samples,
//!   tagged by class and stage.
//! * [`manifest`] — a release pins one versioned set of prompts
//!   (hash of the compiled-in templates), skill versions and the
//!   execution policy version. Exactly one release is active.
//! * [`rollout`] — canary rollout for one release: staged
//!   percentages, a deterministic per-task bucket, and a kill
//!   switch that stops new canary assignments.
//! * [`flags`] — named on/off feature flags read by the CLI entry
//!   points. Turning the new-task entry off never hides in-flight
//!   tasks or already-issued receipts.
//! * [`verify`] — the pre-release checks: old checkpoints must
//!   still restore, flags must be sane, receipts must stay
//!   queryable after a rollback.
//!
//! Nothing here talks to a model or a real account: releases,
//! rollouts and flags are ledger rows; the checks are pure
//! functions over the ledger and the snapshot manifests.

pub mod attribution;
pub mod flags;
pub mod manifest;
pub mod rollout;
pub mod sample;
pub mod verify;

pub use attribution::{classify, FailureClass, FailureEvidence, Stage};
pub use flags::FlagStore;
pub use manifest::{prompt_sha256, Release, ReleaseStore};
pub use rollout::{assign, Rollout, RolloutStore};
pub use sample::{EvalSample, SampleStore};
pub use verify::{check_checkpoint_compat, CheckpointCompat, VerifyCheck};
