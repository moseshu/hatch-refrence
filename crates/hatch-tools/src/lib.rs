// Copyright (c) 2026 Moses (https://github.com/moseshu)
// Author: Moses
// SPDX-License-Identifier: MIT

//! `hatch-tools` — what the assistant can do, and the single path every
//! side effect takes.
//!
//! * [`tool`]      — the `Tool` trait, context and registry.
//! * [`builtin`]   — memory, calendar, mail and proposal tools.
//! * [`executors`] — code that performs approved actions (send mail, create event).
//! * [`dispatch`]  — gate → approval queue → executor, used by both the chat
//!   agent and the triage workflow, so there is exactly one way to cause an
//!   external effect.

pub mod builtin;
pub mod dispatch;
pub mod executors;
pub mod tool;

pub use dispatch::{dispatch_call, route_action, DispatchResult, RouteOutcome};
pub use executors::{
    execute_approved, ActionExecutor, CreateEventExecutor, EmailPayload, EventPayload, ExecOutcome, ExecutorRegistry,
    SendEmailExecutor,
};
pub use tool::{PlannedAction, Registry, Tool, ToolContext, ToolOutput};
