//! `hatch-tools` — what the assistant can do, and the single path every
//! side effect takes.
//!
//! * [`tool`]      — the `Tool` trait, context and registry.
//! * [`builtin`]   — memory, calendar, mail, proposal, file, shell and web tools.
//! * [`workspace`] — the path guard every file tool and command goes through.
//! * [`checkpoint`] — snapshots taken before any file is modified.
//! * [`ws_exec`]   — executors for file writes, sandboxed commands and web access.
//! * [`executors`] — code that performs approved actions (send mail, create event).
//! * [`dispatch`]  — gate → approval queue → executor, used by both the chat
//!   agent and the triage workflow, so there is exactly one way to cause an
//!   external effect.

pub mod builtin;
pub mod checkpoint;
pub mod diff;
pub mod dispatch;
pub mod executors;
pub mod glob;
pub mod tool;
pub mod web;
pub mod workspace;
pub mod ws_exec;

pub use dispatch::{dispatch_call, route_action, DispatchResult, RouteOutcome};
pub use executors::{
    execute_approved, ActionExecutor, CreateEventExecutor, EmailPayload, EventPayload, ExecOutcome, ExecutorRegistry,
    SendEmailExecutor,
};
pub use checkpoint::{Checkpoint, CheckpointStore};
pub use tool::{PlannedAction, Registry, Tool, ToolContext, ToolOutput};
pub use workspace::Workspace;
pub use ws_exec::{ExecCommandExecutor, FetchUrlExecutor, WebSearchExecutor, WriteFileExecutor};
