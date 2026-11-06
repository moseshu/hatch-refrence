//! `hatch-eval` — measure the triage scenario in a sealed sandbox.
//!
//! A *world* is a synthetic persona, mailbox, calendar and expectations. The
//! runner builds an in-memory database and a file outbox, runs triage twice
//! (to test idempotency), approves and executes everything, runs again, and
//! scores classification, reply coverage, injection safety, secret handling,
//! calendar handling and idempotency.
//!
//! Two modes:
//! * **pipeline** — an oracle model answers from the expectations (and plays a
//!   deliberately *fooled* model on adversarial mails). It tests the pipeline
//!   and the structural defences, **not** model quality.
//! * **live** — a real model via the configured router, under a call budget.
//!
//! [`workspace`] holds a second kind of world: a small project directory with
//! file, command and prompt-injection tasks run through the chat agent.
//! [`delegate`] holds a third: sub-agents, cancellation, the depth limit and
//! injected content met by a sub-agent. [`scheduler`] drives the scheduler,
//! daemon and background workers with a fake clock. [`browser`] drives a real
//! Chromium against a local mock site through the chat agent.

pub mod bfcl;
pub mod browser;
pub mod compaction;
pub mod delegate;
pub mod oracle;
pub mod report;
pub mod runner;
pub mod scheduler;
pub mod workspace;
pub mod world;

pub use report::{Check, EvalReport, RepeatedEvalReport};
pub use runner::{run_eval, run_eval_repeat, Mode, RepeatMode};
pub use world::World;
