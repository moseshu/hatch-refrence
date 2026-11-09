//! D2: re-anchoring + trajectory check against a bound plan.
//!
//! The chat turn loop used to never look at plans: the model could wander
//! off the approved steps and nothing noticed until the end (BFCL
//! multi-turn diagnosis: extra reads, missing writes). When a turn is bound
//! to a plan (`AgentConfig::plan_id`), this module keeps it on the rails:
//!
//! * every N steps the plan (goal, steps with their `expected` acceptance
//!   criteria, current step, milestones) is re-injected into the stable
//!   prefix;
//! * after every step the calls made so far are checked against the plan
//!   steps: a call is on-plan when some step names the same tool with the
//!   same arguments. Drift — N consecutive off-plan calls, or a single
//!   off-plan state-changing call — stops the turn and flags the plan
//!   `needs_reapproval` through the existing path. Never silently
//!   continues, never auto-approves.
//!
//! N defaults to [`DEFAULT_REANCHOR_EVERY`] = 2: the measured drift horizon
//! is ~3 rounds, so the anchor must come back before the third round; 1
//! anchors every step (strictest, most tokens). The gate stays the
//! execution guard (a write without approval is queued, never run); this
//! module is the trajectory guard (it stops the turn).

use hatch_plan::{Plan, PlanStep};
use hatch_policy::ActionClass;
use serde_json::Value;

/// Re-anchor every N steps while a plan is bound. Default 2 (see module docs).
pub const DEFAULT_REANCHOR_EVERY: usize = 2;

/// Calls that are mechanism, not work: delegation, skill loading and the
/// tool bridge's own lookup. They neither cover a plan step nor count as
/// drift — the sub-agent's own work is judged by its own brief, and a
/// `find_tool` / `use_tool` call is recorded under the real tool it ran.
const NEUTRAL_CALLS: [&str; 5] = [
    "delegate",
    "delegate_parallel",
    "cancel_subagent",
    "list_skills",
    "load_skill",
];

/// One executed tool call, as the trajectory check sees it. `arguments:
/// None` means "unknown" (eval post-hoc checks only record names); then
/// the name alone decides. `class: None` (unknown tool) counts as a read.
#[derive(Clone, Debug)]
pub struct CallRef {
    pub name: String,
    pub arguments: Option<Value>,
    pub class: Option<ActionClass>,
}

impl CallRef {
    pub fn with_args(name: &str, arguments: Option<Value>, class: Option<ActionClass>) -> Self {
        CallRef {
            name: name.into(),
            arguments,
            class,
        }
    }

    /// Name only (arguments and class unknown).
    pub fn named(name: &str) -> Self {
        CallRef {
            name: name.into(),
            arguments: None,
            class: None,
        }
    }
}

/// Does this action change the world? Reads, local drafts, web fetches
/// and local browser looks do not; everything else does.
pub fn is_state_changing(class: ActionClass) -> bool {
    !matches!(
        class,
        ActionClass::ReadPersonal
            | ActionClass::DraftMessage
            | ActionClass::FetchWeb
            | ActionClass::BrowseLocal
    )
}

/// Does this call carry out this plan step? Same tool, and the arguments
/// agree: unknown call arguments match on the name alone; a step that
/// declares no arguments (`{}` / null) matches any arguments; otherwise
/// the arguments must be deep-equal — the approved plan is the exact
/// action list, not a hint.
pub fn call_matches_step(step: &PlanStep, call: &CallRef) -> bool {
    if step.tool != call.name {
        return false;
    }
    let Some(args) = &call.arguments else {
        return true;
    };
    if step.arguments.is_null() || step.arguments.as_object().is_some_and(|o| o.is_empty()) {
        return true;
    }
    step.arguments == *args
}

/// Which plan steps the calls so far have covered, and which calls went
/// off-plan. Steps are named by id; `off_plan` holds call names in order, deduplicated.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Coverage {
    pub covered: Vec<String>,
    pub uncovered: Vec<String>,
    pub off_plan: Vec<String>,
}

pub fn check_coverage(steps: &[PlanStep], calls: &[CallRef]) -> Coverage {
    let mut covered: Vec<String> = Vec::new();
    let mut off_plan: Vec<String> = Vec::new();
    for call in calls {
        if NEUTRAL_CALLS.contains(&call.name.as_str()) {
            continue;
        }
        match steps.iter().find(|s| call_matches_step(s, call)) {
            Some(s) => {
                if !covered.contains(&s.id) {
                    covered.push(s.id.clone());
                }
            }
            None => {
                if !off_plan.contains(&call.name) {
                    off_plan.push(call.name.clone());
                }
            }
        }
    }
    let uncovered: Vec<String> = steps.iter().filter(|s| !covered.contains(&s.id)).map(|s| s.id.clone()).collect();
    Coverage {
        covered,
        uncovered,
        off_plan,
    }
}

/// A drift finding: why the turn must stop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Drift {
    pub reason: String,
}

/// Walk the calls in order. Drift is either one off-plan state-changing
/// call, or `every` consecutive off-plan calls (the model exploring
/// off-plan — the BFCL pattern). An on-plan call resets the streak;
/// neutral calls neither reset it nor extend it.
pub fn check_drift(steps: &[PlanStep], calls: &[CallRef], every: usize) -> Option<Drift> {
    let every = every.max(1);
    let mut streak = 0usize;
    for call in calls {
        if NEUTRAL_CALLS.contains(&call.name.as_str()) {
            continue;
        }
        if steps.iter().any(|s| call_matches_step(s, call)) {
            streak = 0;
            continue;
        }
        if call.class.is_some_and(is_state_changing) {
            return Some(Drift { reason: format!("计划外的改动调用 `{}`（不在计划步骤里）", call.name) });
        }
        streak += 1;
        if streak >= every {
            return Some(Drift { reason: format!("连续 {streak} 次调用不在计划里（最近一次 `{}`），已偏离计划", call.name) });
        }
    }
    None
}

/// Anchor on step 0 and every `every`-th step after it (`every` of 0 means every step).
pub fn is_anchor_step(step: usize, every: usize) -> bool {
    step.is_multiple_of(every.max(1))
}

/// The plan, rendered for the stable prefix: goal, current step, every step
/// with its acceptance criterion, milestones. `covered` marks the steps
/// already done this turn. Short on purpose — paid on every anchored step.
pub fn render_anchor(plan: &Plan, covered: &[String]) -> String {
    let mut out = format!("## 进行中的计划 {}\n", plan.id);
    if !plan.goal.is_empty() {
        out.push_str(&format!("目标：{}\n", plan.goal));
    }
    out.push_str(&format!("任务：{}\n", plan.task));
    match &plan.current_step {
        Some(id) => {
            let pos = plan.steps.iter().position(|s| &s.id == id).map(|i| i + 1).unwrap_or(0);
            out.push_str(&format!("当前步骤：{id}（第 {pos} / {} 步）\n", plan.steps.len()));
        }
        None => out.push_str("当前步骤：尚未开始\n"),
    }
    out.push_str("步骤（按顺序做，每步做完要达到后面的验收标准）：\n");
    for s in &plan.steps {
        let done = covered.contains(&s.id);
        let cur = plan.current_step.as_deref() == Some(s.id.as_str());
        let args = s.arguments.to_string();
        let args = if args.chars().count() > 120 {
            format!("{}…", args.chars().take(120).collect::<String>())
        } else {
            args
        };
        out.push_str(&format!(
            "- [{}] {} — 工具 `{}`，参数 {} — 验收：{}{}\n",
            if done { "x" } else { " " },
            s.title,
            s.tool,
            args,
            s.expected,
            if cur { " ← 当前步骤" } else { "" }
        ));
    }
    let mut names = |done: bool, label: &str| {
        let ts: Vec<&str> = plan.milestones.iter().filter(|m| m.done == done).map(|m| m.title.as_str()).collect();
        if !ts.is_empty() {
            out.push_str(&format!("{label}：{}\n", ts.join("、")));
        }
    };
    names(false, "未完成里程碑");
    names(true, "已完成里程碑");
    out.push_str("只按上面的步骤做；不在计划里的事不要自己动手，卡住了停下问我，别自己绕路。");
    out
}

/// What a turn did against its plan, for the receipt and eval.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlanTurnReport {
    pub plan_id: String,
    pub covered: Vec<String>,
    pub uncovered: Vec<String>,
    pub off_plan: Vec<String>,
    pub drift: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatch_plan::{RiskLevel, StepStatus};
    use serde_json::json;

    fn step(id: &str, tool: &str, args: Value) -> PlanStep {
        PlanStep {
            id: id.into(),
            title: format!("做 {id}"),
            tool: tool.into(),
            arguments: args,
            files: vec![],
            expected: format!("{id} 做完"),
            risk: RiskLevel::Low,
            status: StepStatus::Pending,
            output: String::new(),
            approval_id: None,
        }
    }

    fn plan() -> Plan {
        // Built through the store so the literal stays small; status is
        // Draft here (render/coverage never check it) — the turn-level
        // bind tests in tests/reanchor.rs cover Approved.
        let db = hatch_store::Db::open_in_memory().unwrap();
        let now = hatch_core::time::now();
        let store = hatch_plan::PlanStore::new(&db);
        let plan = store
            .create(
                "整理资料",
                "资料齐整",
                "",
                vec![
                    step("s1", "memory_search", json!({"query": "x"})),
                    step("s2", "fs_read", json!({"path": "a.txt"})),
                ],
                true,
                &now,
            )
            .unwrap();
        store.set_current_step(&plan.id, Some("s1"), &now).unwrap();
        store.upsert_milestone(&plan.id, "初稿", false, &now).unwrap();
        store.get(&plan.id).unwrap().unwrap()
    }

    fn read(name: &str, args: Value) -> CallRef {
        CallRef::with_args(name, Some(args), Some(ActionClass::ReadPersonal))
    }

    fn write(name: &str, args: Value) -> CallRef {
        CallRef::with_args(name, Some(args), Some(ActionClass::WriteFile))
    }

    #[test]
    fn anchor_step_interval() {
        assert!(is_anchor_step(0, 2));
        assert!(!is_anchor_step(1, 2));
        assert!(is_anchor_step(2, 2));
        assert!(is_anchor_step(4, 2));
        assert!(is_anchor_step(3, 1), "N=1 anchors every step");
        assert!(is_anchor_step(5, 0), "N=0 normalises to 1");
    }

    #[test]
    fn render_anchor_names_goal_steps_and_milestones() {
        let text = render_anchor(&plan(), &["s1".to_string()]);
        assert!(text.contains("资料齐整"), "{text}");
        assert!(text.contains("整理资料"), "{text}");
        assert!(text.contains("做 s1") && text.contains("s1 做完"), "{text}");
        assert!(text.contains("做 s2") && text.contains("s2 做完"), "{text}");
        assert!(text.contains("初稿"), "{text}");
        assert!(text.contains("← 当前步骤"), "{text}");
        assert!(text.contains("[x] 做 s1"), "{text}");
        assert!(text.contains("[ ] 做 s2"), "{text}");
    }

    #[test]
    fn coverage_matches_name_and_args() {
        let steps = plan().steps;
        let calls = vec![
            read("memory_search", json!({"query": "x"})),
            CallRef::named("delegate"),
            off("memory_search"),
            off("web_search"),
        ];
        let cov = check_coverage(&steps, &calls);
        assert_eq!(cov.covered, vec!["s1"]);
        assert_eq!(cov.uncovered, vec!["s2"]);
        assert_eq!(cov.off_plan, vec!["memory_search", "web_search"]);
    }

    #[test]
    fn coverage_empty_step_args_and_unknown_call_args() {
        let steps = vec![step("s1", "memory_search", json!({}))];
        // Step declares no arguments: any arguments match.
        let cov = check_coverage(&steps, &[read("memory_search", json!({"query": "anything"}))]);
        assert_eq!(cov.covered, vec!["s1"]);
        // Call arguments unknown (eval name-only): name alone decides.
        let cov = check_coverage(&steps, &[CallRef::named("memory_search")]);
        assert_eq!(cov.covered, vec!["s1"]);
        let cov = check_coverage(&steps, &[CallRef::named("web_search")]);
        assert!(cov.covered.is_empty() && cov.off_plan == vec!["web_search"]);
    }

    fn off(name: &str) -> CallRef {
        // An off-plan read with dummy arguments (name matches no step's args).
        read(name, json!({"query": "off-plan"}))
    }

    #[test]
    fn drift_needs_n_consecutive_off_plan_reads() {
        let steps = plan().steps;
        assert!(check_drift(&steps, &[off("memory_search")], 2).is_none(), "one off-plan read is tolerated");
        let two = vec![off("memory_search"), off("web_search")];
        let d = check_drift(&steps, &two, 2).unwrap();
        assert!(d.reason.contains("连续 2 次"), "{}", d.reason);
        // An on-plan call resets the streak.
        let reset = vec![
            off("memory_search"),
            read("memory_search", json!({"query": "x"})),
            off("web_search"),
        ];
        assert!(check_drift(&steps, &reset, 2).is_none());
        // Neutral calls neither reset nor extend the streak.
        let neutral = vec![off("memory_search"), CallRef::named("delegate"), off("web_search")];
        assert!(check_drift(&steps, &neutral, 2).is_some());
    }

    #[test]
    fn drift_on_first_off_plan_write() {
        let steps = plan().steps;
        let d = check_drift(&steps, &[write("fs_write", json!({"path": "x"}))], 2).unwrap();
        assert!(d.reason.contains("改动调用"), "{}", d.reason);
        // Unknown class counts as a read: no immediate trigger.
        let unknown = vec![CallRef::named("fs_write")];
        assert!(check_drift(&steps, &unknown, 2).is_none());
        let unknown2 = vec![CallRef::named("fs_write"), CallRef::named("fs_write")];
        assert!(check_drift(&steps, &unknown2, 2).is_some(), "two unknowns still drift");
    }

    #[test]
    fn state_changing_classes() {
        for c in [
            ActionClass::ReadPersonal,
            ActionClass::DraftMessage,
            ActionClass::FetchWeb,
            ActionClass::BrowseLocal,
        ] {
            assert!(!is_state_changing(c), "{c:?}");
        }
        for c in [
            ActionClass::WriteMemory,
            ActionClass::SendMessage,
            ActionClass::WriteCalendar,
            ActionClass::DeleteData,
            ActionClass::Payment,
            ActionClass::WriteFile,
            ActionClass::ExecCommand,
            ActionClass::BrowserSubmit,
        ] {
            assert!(is_state_changing(c), "{c:?}");
        }
    }
}
