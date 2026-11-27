//! Sub-command implementations, one module per command group.

pub mod agents;
pub mod approval_service;
#[cfg(test)]
mod approval_service_drill;
#[cfg(test)]
mod approval_service_drill_helpers;
pub mod approvals;
pub mod artifact;
pub mod brief;
pub mod browser;
pub mod calendar;
pub mod chat;
pub(crate) mod wake;
#[cfg(test)]
mod wake_tests;
pub(crate) mod chat_drain;
pub mod checkpoint;
pub mod connectors;
pub mod daemon;
pub mod daemon_token;
pub mod digest;
pub mod doctor;
pub mod eval;
pub mod eval_compaction;
mod eval_record;
pub mod eval_p1;
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
pub mod questions;
pub mod release;
#[cfg(test)]
mod release_drill;
#[cfg(test)]
mod release_drill_helpers;
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
pub mod task_delegate_reconcile;
#[cfg(test)]
mod task_delegation_compare_drill;
#[cfg(test)]
mod task_delegation_drill;
#[cfg(test)]
mod task_delegation_drill_helpers;
#[cfg(test)]
mod task_delegation_merge_drill;
#[cfg(test)]
mod task_desensitize_drill;
pub mod task_edit;
pub mod task_gather;
pub mod task_gmail;
#[cfg(test)]
mod task_gmail_drill;
pub mod task_invite;
#[cfg(test)]
mod task_invite_calendar_drill;
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
pub mod task_command_edit;
#[cfg(test)]
mod task_failure_drill;
pub mod task_orchestration;
pub mod task_orchestration_tools;
#[cfg(test)]
mod task_orchestration_tests;
pub mod task_plan_mirror;
pub mod task_plan_runner;
pub mod task_react;
pub mod task_react_turn;
pub mod task_replan;
#[cfg(test)]
mod p1_plan_drill;
#[cfg(test)]
mod task_command_edit_tests;
#[cfg(test)]
mod p1_react_drill;
#[cfg(test)]
mod p1_submit_drill;
pub mod task_resident;
#[cfg(test)]
mod task_resident_drill;
pub mod task_routing;
pub mod task_run_params;
pub mod task_run_params_child;
#[cfg(test)]
mod task_run_params_tests;
pub mod task_run_params_gmail;
pub mod task_steps;
pub(crate) mod task_turn;
pub(crate) mod task_version;
pub mod task_view;
pub mod task_worker;
#[cfg(test)]
mod task_drill;
pub mod tools;
pub mod transport;
pub mod triage;
pub mod vault;
