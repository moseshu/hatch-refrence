//! Pipeline-only sequence checks for the delegation world.
//!
//! The expectations pin the oracle's scripted trajectory (see
//! [`crate::sequence`]), so these checks only run when the model *is*
//! the script. In live mode a real model may take a different, equally
//! valid route, and the same expectations would fail it for being
//! creative rather than wrong. Live mode keeps the result-state checks
//! in `report::score`; judging a live model's trajectory against a plan
//! is P1 work (D2), not this check.
//!
//! Runs compare SORTED, not in spawn order: parallel children finish in
//! timing order, not spawn order (verified: the split runs came back
//! auth/billing/search on one run and billing/auth/search on the next),
//! so their interleaving is not a contract. The sort key pins the *set*
//! of trajectories; sequential runs keep their relative order through
//! it (depth 1 sorts before depth 2). Spawn order itself is not
//! asserted anywhere: `assert_turn_order` exists for golden threads
//! (P1 #19), where the recorded order is the contract.

use super::report::{RunBrief, TaskResult};
use super::DelegateSpec;
use crate::report::Check;
use crate::sequence::{
    assert_handoff_tree, assert_tool_calls, assert_tool_names, HandoffNode, SeqCall,
};

fn check(name: &str, passed: bool, detail: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        passed,
        detail: detail.into(),
    }
}

fn task<'a>(tasks: &'a [TaskResult], id: &str) -> &'a TaskResult {
    tasks.iter().find(|t| t.id == id).expect("task present")
}

/// Stable sort key for a run: the set of trajectories is pinned, their
/// interleaving is not.
fn run_key(
    agent_type: &str,
    depth: usize,
    status: &str,
    calls: &[SeqCall],
) -> (String, usize, String, String) {
    let calls = calls
        .iter()
        .map(|c| {
            format!(
                "{}={}",
                c.name,
                c.arguments
                    .as_ref()
                    .map(|v| v.to_string())
                    .unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    (agent_type.to_string(), depth, status.to_string(), calls)
}

pub fn sequence_checks(spec: &DelegateSpec, tasks: &[TaskResult], out: &mut Vec<Check>) {
    for (id, want) in &spec.expected_parent_tools {
        let t = task(tasks, id);
        let (ok, detail) = assert_tool_names(&t.tool_calls, want);
        out.push(check(&format!("{id}：父代理工具调用顺序"), ok, detail));
    }
    for (id, want_runs) in &spec.expected_runs {
        let t = task(tasks, id);
        let mut runs: Vec<&RunBrief> = t.runs.iter().collect();
        runs.sort_by_key(|r| run_key(&r.agent_type, r.depth, &r.status, &r.calls));
        let mut want: Vec<&super::ExpectedRun> = want_runs.iter().collect();
        want.sort_by_key(|r| {
            run_key(
                &r.agent_type,
                r.depth,
                &r.status,
                &r.tool_calls
                    .iter()
                    .map(|c| SeqCall {
                        name: c.name.clone(),
                        arguments: c.arguments.clone(),
                    })
                    .collect::<Vec<_>>(),
            )
        });
        let nodes: Vec<HandoffNode> = runs
            .iter()
            .map(|r| HandoffNode {
                agent: r.agent_type.clone(),
                depth: r.depth,
                status: r.status.clone(),
            })
            .collect();
        let want_nodes: Vec<HandoffNode> = want
            .iter()
            .map(|r| HandoffNode {
                agent: r.agent_type.clone(),
                depth: r.depth,
                status: r.status.clone(),
            })
            .collect();
        let (ok, detail) = assert_handoff_tree(&nodes, &want_nodes);
        out.push(check(&format!("{id}：子代理树"), ok, detail));
        let actual: Vec<SeqCall> = runs.iter().flat_map(|r| r.calls.clone()).collect();
        let want_calls: Vec<SeqCall> = want
            .iter()
            .flat_map(|r| r.tool_calls.clone())
            .map(|c| SeqCall {
                name: c.name,
                arguments: c.arguments,
            })
            .collect();
        let (ok, detail) = assert_tool_calls(&actual, &want_calls);
        out.push(check(&format!("{id}：子代理工具调用序列"), ok, detail));
    }
}
