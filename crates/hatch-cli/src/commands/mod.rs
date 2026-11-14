//! Sub-command implementations, one module per command group.

pub mod agents;
pub mod approvals;
pub mod artifact;
pub mod brief;
pub mod browser;
pub mod calendar;
pub mod chat;
pub(crate) mod chat_drain;
pub mod checkpoint;
pub mod connectors;
pub mod daemon;
pub mod daemon_token;
pub mod digest;
pub mod doctor;
pub mod eval;
pub mod eval_compaction;
pub mod feed;
pub mod goal;
pub mod grants;
pub mod ideas;
pub mod inference;
pub mod init;
pub mod jobs;
pub mod mcp;
pub mod mcp_auth;
pub mod mcp_token;
pub mod memory;
pub mod plan;
pub mod plan_show;
pub mod subagent_run;
pub(crate) mod subagent_test_env;
pub mod session;
pub mod skills;
pub mod task;
#[cfg(test)]
mod task_approval_drill;
pub mod task_control;
#[cfg(test)]
mod task_control_drill;
pub mod task_delegate;
#[cfg(test)]
mod task_delegation_compare_drill;
#[cfg(test)]
mod task_delegation_drill;
#[cfg(test)]
mod task_desensitize_drill;
pub mod task_edit;
pub mod task_gather;
pub mod task_gmail;
#[cfg(test)]
mod task_gmail_drill;
pub mod task_invite;
#[cfg(test)]
mod task_invite_drill;
#[cfg(test)]
mod task_list_drill;
#[cfg(test)]
mod task_pause_drill;
pub mod task_kinds;
#[cfg(test)]
mod task_compression_drill;
#[cfg(test)]
mod task_context_drill;
#[cfg(test)]
mod task_failure_drill;
pub mod task_replan;
pub mod task_steps;
pub(crate) mod task_version;
pub mod task_view;
pub mod task_worker;
#[cfg(test)]
mod task_drill;
pub mod tools;
pub mod transport;
pub mod triage;
pub mod vault;
