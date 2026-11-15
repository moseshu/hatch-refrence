//! Child-task execution parameters (P1): a child inherits its
//! parent's run parameters, re-derived for the child's own kind.
//! Split out of `task_run_params` so that file stays under the
//! line-count rule.

use super::task_routing::mode_for_kind;
use super::task_run_params::RunParams;
use hatch_core::time::Timestamp;
use hatch_core::Result;
use hatch_store::Db;

/// Copy a parent task's execution parameters onto a child (task 7):
/// the world, source, account and scope stay the same, so the
/// resident worker can run the child with no further input. The
/// output dir stays unset, so the child gets its own per-task dir.
/// The mode is re-derived from the CHILD's kind (a brief child of a
/// delegate-join parent runs fixed-flow, not durable-subtasks), and
/// the ReAct caps reset to the runner defaults.
pub(crate) fn inherit_params_for_child(
    db: &Db,
    parent_id: &str,
    child_id: &str,
    kind: &str,
    title: &str,
    now: &Timestamp,
) -> Result<()> {
    let Some(mut params) = RunParams::load(db, parent_id)? else {
        return Ok(());
    };
    params.kind = kind.to_string();
    params.title = title.to_string();
    params.out_dir = None;
    params.mode = mode_for_kind(kind)
        .map(|m| m.as_str().to_string())
        .unwrap_or_default();
    params.react_max_steps = 0;
    params.react_max_tool_calls = 0;
    params.react_timeout_secs = 0;
    params.save(db, child_id, now)?;
    Ok(())
}
