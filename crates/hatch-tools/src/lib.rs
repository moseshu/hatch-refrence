//! `hatch-tools` — what the assistant can do, and the single path every
//! side effect takes.
//!
//! * [`tool`]      — the `Tool` trait, context and registry.
//! * [`builtin`]   — memory, calendar, mail, proposal, file, shell and web tools.
//! * [`workspace`] — the path guard every file tool and command goes through.
//! * [`checkpoint`] — snapshots taken before any file is modified.
//! * [`ws_exec`]   — executors for file writes, sandboxed commands and web access.
//! * [`browser_exec`] — executors for approved browser actions (open, click / submit).
//! * [`executors`] — code that performs approved actions (send mail, create event).
//! * [`dispatch`]  — gate → approval queue → executor, used by both the chat
//!   agent and the triage workflow, so there is exactly one way to cause an
//!   external effect.

pub mod ask_exec;
pub mod browser_exec;
pub mod builtin;
pub mod checkpoint;
pub mod connector;
pub mod diff;
pub mod dispatch;
pub mod executors;
mod evidence_readers;
pub mod glob;
pub mod google_event;
pub mod intent;
pub mod schedule_exec;
pub mod tool;
pub mod web;
pub mod verification;
pub mod workspace;
pub mod ws_exec;

pub use ask_exec::AskUserExecutor;
pub use browser_exec::{BrowserClickExecutor, BrowserOpenExecutor};
pub use checkpoint::{Checkpoint, CheckpointStore};
pub use connector::{connector_tool, connector_tools, ConnectorCallExecutor, ConnectorTool};
pub use dispatch::{dispatch_call, route_action, route_action_with, DispatchResult, RouteOutcome};
pub use executors::{
    execute_approved, execute_approved_with, ActionExecutor, CreateEventExecutor, EmailPayload,
    EventPayload, ExecContext, ExecOutcome, ExecutorRegistry, SendEmailExecutor,
};
pub use google_event::CreateGoogleEventExecutor;
pub use intent::{
    Counterfactual, IntentFailure, IntentGateMode, IntentJudge, IntentJudgeInput, IntentJudgement,
    IntentReview, IntentVerdict, IntentVerdictStore, PreparedActionView, VerdictRecord,
};
pub use schedule_exec::{CreateJobExecutor, DeleteJobExecutor};
pub use tool::{ImageAttachment, PlannedAction, Registry, Tool, ToolContext, ToolOutput};
pub use workspace::Workspace;
pub use ws_exec::{ExecCommandExecutor, FetchUrlExecutor, WebSearchExecutor, WriteFileExecutor};
