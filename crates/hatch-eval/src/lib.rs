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
//! Chromium against a local mock site through the chat agent. [`memory`]
//! runs simulated days with the nightly memory extraction between them and
//! checks that stated preferences, corrections and refusals carry across days.
//! [`scheduled`] fires user-scheduled `agent_prompt` jobs through the real
//! engine on a fake clock and checks what they deliver while unattended.
//! [`caution`] pairs clear requests that must be done at once with ambiguous
//! or planted ones that must wait for the user, scoring both directions.
//! [`faults`] wraps real tools so chosen calls fail (transient, denied,
//! persistent, lost write response) and checks how the agent copes.

pub mod browser;
pub mod caution;
pub mod checks;
pub mod compaction;
pub mod dates;
pub mod delegate;
pub mod faults;
pub mod markers;
pub mod meeting_brief;
pub mod memory;
pub mod oracle;
pub mod recording;
pub mod replay;
pub mod report;
pub mod runner;
pub mod scheduled;
pub mod scorecard;
pub mod scheduler;
pub mod sequence;
pub mod variant;
pub mod weblive;
pub mod workspace;
pub mod world;

pub use report::{Check, CheckFlags, EvalReport, RepeatedEvalReport};
pub use runner::{run_eval, run_eval_repeat, Mode, RepeatMode};
pub use world::{Expectations, Persona, World};
