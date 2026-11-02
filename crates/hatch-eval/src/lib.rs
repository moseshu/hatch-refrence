// Copyright (c) 2026 Moses (https://github.com/moseshu)
// Author: Moses
// SPDX-License-Identifier: MIT

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

pub mod oracle;
pub mod report;
pub mod runner;
pub mod world;

pub use report::{Check, EvalReport};
pub use runner::{run_eval, Mode};
pub use world::World;
