//! Deterministic routing by risk and task type (P1 generic
//! orchestration, plan-3 §6.1 / §10.1).
//!
//! The router is a pure function over goal features — never "the
//! model decides". The model may PROPOSE a mode when it submits a
//! goal, but the runtime validates the proposal against the same
//! features and refuses a mismatch with a reason instead of running
//! it. Every goal shape in the tests maps to exactly one mode.
//!
//! Precedence (from the plan's routing table):
//! 1. independently parallelizable + multi-stage → durable subtasks
//! 2. multi-stage, or approval / dependency / external submit / long
//!    wait / a plan draft → Plan-and-Execute
//! 3. a single observation-driven problem (search, browse, diagnose)
//!    → bounded ReAct
//! 4. steps known (a fixed flow kind) → fixed flow
//! 5. otherwise → direct answer (no durable task at all)

use hatch_core::Error;
use serde_json::Value;

/// The five execution modes of §6.1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionMode {
    /// Simple Q&A / one read: the main agent answers directly.
    DirectAnswer,
    /// A known deterministic flow (meeting brief, invite, weekly
    /// report): the worker kind runs it.
    FixedFlow,
    /// One observation-driven problem inside a single step, with
    /// per-step model-call / tool / time caps.
    BoundedReact,
    /// Multi-stage with approval, dependency, external submit or a
    /// long wait: a plan is registered first, then executed.
    PlanAndExecute,
    /// Independently verifiable, parallelizable sub-problems: the
    /// task-7 delegation ledger runs them.
    DurableSubtasks,
}

impl ExecutionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ExecutionMode::DirectAnswer => "direct-answer",
            ExecutionMode::FixedFlow => "fixed-flow",
            ExecutionMode::BoundedReact => "bounded-react",
            ExecutionMode::PlanAndExecute => "plan-and-execute",
            ExecutionMode::DurableSubtasks => "durable-subtasks",
        }
    }

    pub fn parse(s: &str) -> Option<ExecutionMode> {
        match s {
            "direct-answer" => Some(ExecutionMode::DirectAnswer),
            "fixed-flow" => Some(ExecutionMode::FixedFlow),
            "bounded-react" => Some(ExecutionMode::BoundedReact),
            "plan-and-execute" => Some(ExecutionMode::PlanAndExecute),
            "durable-subtasks" => Some(ExecutionMode::DurableSubtasks),
            _ => None,
        }
    }
}

/// The goal features the router reads. Built by the submit path from
/// the request (kind, plan draft, subtasks, observe flag) — never
/// guessed from free text alone.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GoalFeatures {
    /// The goal performs an external side effect (send, create).
    pub has_external_side_effect: bool,
    /// More than one stage, or a plan draft was supplied.
    pub multi_stage: bool,
    /// A step needs human approval before it runs.
    pub needs_approval: bool,
    /// Steps depend on each other or on an earlier result.
    pub has_dependency: bool,
    /// A step submits something externally (mail, form, API).
    pub has_external_submit: bool,
    /// The task waits on something long (a reply, a time, a child).
    pub has_long_wait: bool,
    /// Sub-problems are independently verifiable and parallelizable.
    pub independently_parallelizable: bool,
    /// The steps are known (a fixed-flow kind was named).
    pub steps_known: bool,
    /// One problem that needs observation to proceed (search, browse
    /// an unseen page, diagnose).
    pub single_observation_driven: bool,
    /// A plan draft (steps) was supplied with the goal.
    pub has_plan_draft: bool,
    /// Subtasks were supplied with the goal.
    pub has_subtasks: bool,
}

/// Route a goal to its one execution mode, from its features only.
pub fn route_goal(f: &GoalFeatures) -> ExecutionMode {
    if f.independently_parallelizable && f.multi_stage {
        return ExecutionMode::DurableSubtasks;
    }
    if f.multi_stage
        || f.needs_approval
        || f.has_dependency
        || f.has_external_submit
        || f.has_long_wait
        || f.has_plan_draft
    {
        return ExecutionMode::PlanAndExecute;
    }
    if f.single_observation_driven {
        return ExecutionMode::BoundedReact;
    }
    if f.steps_known {
        return ExecutionMode::FixedFlow;
    }
    ExecutionMode::DirectAnswer
}

/// The mode a worker kind runs under. `None` for an unknown kind —
/// the kind registry, not the router, owns that refusal.
pub fn mode_for_kind(kind: &str) -> Option<ExecutionMode> {
    match kind {
        "meeting-brief" | "meeting-invite" | "weekly-report" => Some(ExecutionMode::FixedFlow),
        "react" => Some(ExecutionMode::BoundedReact),
        "plan" => Some(ExecutionMode::PlanAndExecute),
        "delegate-join" => Some(ExecutionMode::DurableSubtasks),
        _ => None,
    }
}

/// Validate a persisted mode against the task's kind at dispatch
/// time (P1): a task whose stored mode disagrees with its kind is
/// misfiled — the worker blocks it with a reason instead of running
/// the wrong shape. Kinds the router does not know accept any mode
/// (their runner validates for itself).
pub fn validate_mode_for_kind(kind: &str, mode: &str) -> Result<(), String> {
    let Some(expected) = mode_for_kind(kind) else {
        return Ok(());
    };
    let parsed = ExecutionMode::parse(mode)
        .ok_or_else(|| format!("任务 {kind} 的执行方式“{mode}”无效"))?;
    if parsed != expected {
        return Err(format!(
            "任务 kind {kind} 应以 {} 方式执行，但记录的执行方式是 {}：请重新提交",
            expected.as_str(),
            parsed.as_str()
        ));
    }
    Ok(())
}

/// Parse the routed execution mode + bounded-ReAct caps out of a
/// task-create body (P1, §6.1). An unknown mode name or an
/// out-of-range cap is refused here, not at run time. 0 keeps the
/// runner default (6 / 20 / 300).
pub fn parse_mode_caps(body: &Value) -> hatch_core::Result<(String, u64, u64, u64)> {
    let mode = body
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if !mode.is_empty() && ExecutionMode::parse(&mode).is_none() {
        return Err(Error::Invalid(format!(
            "未知执行方式 {mode}（fixed-flow/bounded-react/plan-and-execute/durable-subtasks）"
        )));
    }
    let react_max_steps = body.get("react_max_steps").and_then(|v| v.as_u64()).unwrap_or(0);
    if react_max_steps > 12 {
        return Err(Error::Invalid("react_max_steps 最多 12".into()));
    }
    let react_max_tool_calls = body
        .get("react_max_tool_calls")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    if react_max_tool_calls > 100 {
        return Err(Error::Invalid("react_max_tool_calls 最多 100".into()));
    }
    let react_timeout_secs = body
        .get("react_timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    if react_timeout_secs > 1800 {
        return Err(Error::Invalid("react_timeout_secs 最多 1800".into()));
    }
    Ok((mode, react_max_steps, react_max_tool_calls, react_timeout_secs))
}

/// Validate a proposed mode against the goal's features and kind.
/// `Ok` means the runtime may run it; `Err` names the expected mode
/// and why, so the caller can refuse instead of running.
pub fn validate_mode(
    proposed: ExecutionMode,
    features: &GoalFeatures,
    kind: &str,
) -> Result<(), String> {
    let expected = route_goal(features);
    if proposed != expected {
        return Err(format!(
            "执行方式 {} 与目标特征不符：按特征应为 {}（{}）",
            proposed.as_str(),
            expected.as_str(),
            why(expected, features)
        ));
    }
    // The routed mode also needs its structure: a plan needs a draft,
    // subtasks need subtasks, a fixed flow needs known steps, a ReAct
    // step needs an observation-driven problem.
    match proposed {
        ExecutionMode::PlanAndExecute if !features.has_plan_draft => {
            return Err(
                "Plan-and-Execute 需要计划草案（steps）：先给出首批步骤再提交".to_string(),
            );
        }
        ExecutionMode::DurableSubtasks if !features.has_subtasks => {
            return Err("持久子任务需要子任务列表（subtasks）：先给出可并行的子问题再提交".into());
        }
        ExecutionMode::FixedFlow if !features.steps_known => {
            return Err("固定流程需要已知的任务 kind：先指明要跑哪种固定流程".into());
        }
        ExecutionMode::BoundedReact if !features.single_observation_driven => {
            return Err("有界 ReAct 需要一个依赖观察的单一问题：先指明要打开的页面或要排查的问题".into());
        }
        _ => {}
    }
    if !kind.is_empty() {
        match mode_for_kind(kind) {
            Some(km) if km != proposed => {
                return Err(format!(
                    "任务 kind {kind} 按 {mode} 执行，与提议的 {proposed} 不符",
                    mode = km.as_str(),
                    proposed = proposed.as_str()
                ));
            }
            None => {
                return Err(format!("未知任务 kind {kind}"));
            }
            _ => {}
        }
    }
    Ok(())
}

/// One line saying why a mode was routed, for the refusal message.
fn why(mode: ExecutionMode, f: &GoalFeatures) -> &'static str {
    match mode {
        ExecutionMode::DurableSubtasks => "子问题可独立验证且可并行",
        ExecutionMode::PlanAndExecute => {
            if f.has_plan_draft {
                "目标附带了计划草案"
            } else if f.needs_approval {
                "目标包含需审批的步骤"
            } else if f.has_external_submit {
                "目标包含外部提交"
            } else if f.has_long_wait {
                "目标包含长等待"
            } else if f.has_dependency {
                "步骤之间有依赖"
            } else {
                "目标是多阶段的"
            }
        }
        ExecutionMode::BoundedReact => "单一问题依赖即时观察",
        ExecutionMode::FixedFlow => "步骤已知的固定流程",
        ExecutionMode::DirectAnswer => "简单问答，直接回答即可",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_goal_shape_routes_to_exactly_one_mode() {
        // Simple Q&A: direct answer.
        assert_eq!(route_goal(&GoalFeatures::default()), ExecutionMode::DirectAnswer);
        // Steps known, nothing else: fixed flow.
        let f = GoalFeatures { steps_known: true, ..Default::default() };
        assert_eq!(route_goal(&f), ExecutionMode::FixedFlow);
        // One observation-driven problem: bounded ReAct — even when
        // the steps are also known, observation wins.
        let f = GoalFeatures { single_observation_driven: true, steps_known: true, ..Default::default() };
        assert_eq!(route_goal(&f), ExecutionMode::BoundedReact);
        // Multi-stage / approval / external submit / long wait /
        // dependency / plan draft: Plan-and-Execute.
        for f in [
            GoalFeatures { multi_stage: true, ..Default::default() },
            GoalFeatures { needs_approval: true, ..Default::default() },
            GoalFeatures { has_external_submit: true, ..Default::default() },
            GoalFeatures { has_long_wait: true, ..Default::default() },
            GoalFeatures { has_dependency: true, ..Default::default() },
            GoalFeatures { has_plan_draft: true, ..Default::default() },
        ] {
            assert_eq!(route_goal(&f), ExecutionMode::PlanAndExecute, "{f:?}");
        }
        // Parallelizable + multi-stage: durable subtasks — it wins
        // over Plan-and-Execute.
        let f = GoalFeatures {
            independently_parallelizable: true,
            multi_stage: true,
            needs_approval: true,
            has_subtasks: true,
            ..Default::default()
        };
        assert_eq!(route_goal(&f), ExecutionMode::DurableSubtasks);
        // Parallelizable alone (no multi-stage) is not enough: a
        // single parallelizable problem is still a ReAct problem.
        let f = GoalFeatures {
            independently_parallelizable: true,
            single_observation_driven: true,
            ..Default::default()
        };
        assert_eq!(route_goal(&f), ExecutionMode::BoundedReact);
    }

    #[test]
    fn mode_for_kind_covers_every_known_kind() {
        assert_eq!(mode_for_kind("meeting-brief"), Some(ExecutionMode::FixedFlow));
        assert_eq!(mode_for_kind("meeting-invite"), Some(ExecutionMode::FixedFlow));
        assert_eq!(mode_for_kind("weekly-report"), Some(ExecutionMode::FixedFlow));
        assert_eq!(mode_for_kind("react"), Some(ExecutionMode::BoundedReact));
        assert_eq!(mode_for_kind("plan"), Some(ExecutionMode::PlanAndExecute));
        assert_eq!(mode_for_kind("delegate-join"), Some(ExecutionMode::DurableSubtasks));
        assert_eq!(mode_for_kind("nope"), None);
    }

    #[test]
    fn an_invalid_mode_is_refused_with_a_reason() {
        // Proposing fixed-flow for a multi-stage goal: refused, and
        // the message names the expected mode.
        let f = GoalFeatures { multi_stage: true, has_plan_draft: true, ..Default::default() };
        let err = validate_mode(ExecutionMode::FixedFlow, &f, "").unwrap_err();
        assert!(err.contains("plan-and-execute"), "{err}");
        // Proposing Plan-and-Execute without a plan draft: refused.
        let f = GoalFeatures { multi_stage: true, ..Default::default() };
        let err = validate_mode(ExecutionMode::PlanAndExecute, &f, "").unwrap_err();
        assert!(err.contains("计划草案"), "{err}");
        // Proposing durable subtasks without subtasks: refused.
        let f = GoalFeatures {
            independently_parallelizable: true,
            multi_stage: true,
            ..Default::default()
        };
        let err = validate_mode(ExecutionMode::DurableSubtasks, &f, "").unwrap_err();
        assert!(err.contains("子任务列表"), "{err}");
        // Proposing bounded ReAct for a plain question: refused —
        // a plain question routes to direct-answer, not ReAct.
        let err =
            validate_mode(ExecutionMode::BoundedReact, &GoalFeatures::default(), "").unwrap_err();
        assert!(err.contains("direct-answer"), "{err}");
        // Kind and mode disagreeing: refused even when the features
        // agree with the proposed mode.
        let f = GoalFeatures { single_observation_driven: true, ..Default::default() };
        let err = validate_mode(ExecutionMode::BoundedReact, &f, "meeting-brief").unwrap_err();
        assert!(err.contains("meeting-brief"), "{err}");
        // Unknown kind: refused (with features that do route to the
        // proposed mode, so the kind check is the one that fires).
        let f = GoalFeatures { steps_known: true, ..Default::default() };
        let err = validate_mode(ExecutionMode::FixedFlow, &f, "nope").unwrap_err();
        assert!(err.contains("未知任务 kind"), "{err}");
    }

    #[test]
    fn a_valid_proposal_passes() {
        let f = GoalFeatures {
            multi_stage: true,
            has_plan_draft: true,
            ..Default::default()
        };
        assert!(validate_mode(ExecutionMode::PlanAndExecute, &f, "plan").is_ok());
        let f = GoalFeatures { single_observation_driven: true, ..Default::default() };
        assert!(validate_mode(ExecutionMode::BoundedReact, &f, "react").is_ok());
        let f = GoalFeatures { steps_known: true, ..Default::default() };
        assert!(validate_mode(ExecutionMode::FixedFlow, &f, "meeting-brief").is_ok());
        // No kind given: only the features are checked.
        assert!(validate_mode(ExecutionMode::FixedFlow, &f, "").is_ok());
    }
}
