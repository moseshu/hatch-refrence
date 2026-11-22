//! Generic orchestration, chat side (P1, plan-3 §6.1 / §10.1):
//! natural language → durable goal.
//!
//! The chat agent submits a goal through the `submit_goal` tool
//! instead of answering inline: the goal is routed by the
//! deterministic router ([`task_routing`]), persisted on the task
//! ledger with its execution parameters, and the resident worker
//! runs it — no `hatch task run`, no manual API call. The
//! `task_status` tool reads the same ledger projection as
//! `task show`, so "查一下现在做到哪" answers from the task list,
//! never from a summary.
//!
//! Layering (the M1 rule): hatch-tools must not depend on
//! hatch-tasks, so these tools live here in hatch-cli — the only
//! crate that sees tasks + approvals + executors + connectors —
//! following the P0 #1 `ApprovalService` precedent. The tools are
//! stateless: they work from the [`ToolContext`]'s database.

use super::task_delegate::ensure_delegation_step;
use super::task_routing::{
    route_goal, validate_mode, ExecutionMode, GoalFeatures,
};
use super::task_run_params::RunParams;
use hatch_core::time::Timestamp;
use hatch_core::{Error, Result};
use hatch_plan::acceptance::{nl_items, validate as validate_acceptance, AcceptanceItem};
use hatch_plan::{PlanStep, PlanStore};
use hatch_store::Db;
use hatch_tasks::{
    ChildSpec, CreateOutcome, DelegationStore, TaskContext, TaskContextStore, TaskStore,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// One subtask of a durable-subtasks goal.
#[derive(Clone, Debug)]
pub struct SubtaskRequest {
    pub title: String,
    pub kind: String,
    pub budget: u64,
}

/// What `submit_goal` carries. `kind` is the fixed-flow kind when
/// the goal names one; `mode` is the model's proposed mode (the
/// runtime validates it); `steps` is the plan draft for
/// Plan-and-Execute; `subtasks` are the parallel sub-problems for
/// durable subtasks; `observe` marks a single observation-driven
/// problem (or the goal names the page to open).
#[derive(Clone, Debug, Default)]
pub struct SubmitRequest {
    pub goal: String,
    pub title: Option<String>,
    pub kind: Option<String>,
    pub mode: Option<String>,
    pub acceptance: Vec<String>,
    /// Structured acceptance items (predicates / NL) for the frozen
    /// task-level contract; string `acceptance` entries are appended
    /// as NL items for compatibility.
    pub acceptance_items: Vec<AcceptanceItem>,
    pub idempotency_key: Option<String>,
    pub steps: Option<Value>,
    pub subtasks: Vec<SubtaskRequest>,
    pub observe: bool,
    pub prompt: Option<String>,
    pub world: Option<String>,
    pub source: String,
    pub gmail_user: Option<String>,
    pub gmail_name: Option<String>,
    pub invite: Option<Value>,
    pub max_model_calls: u64,
}

impl SubmitRequest {
    /// Parse the `submit_goal` tool arguments.
    pub fn from_args(args: &Value) -> Result<SubmitRequest> {
        let goal = args
            .get("goal")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Invalid("缺少 goal（自然语言目标）".into()))?
            .to_string();
        let subtasks = match args.get("subtasks").and_then(Value::as_array) {
            Some(list) => {
                let mut out = Vec::new();
                for (i, v) in list.iter().enumerate() {
                    let title = v
                        .get("title")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| Error::Invalid(format!("subtasks[{i}] 缺少 title")))?
                        .to_string();
                    let kind = v
                        .get("kind")
                        .and_then(|k| k.as_str())
                        .unwrap_or("meeting-brief")
                        .to_string();
                    let budget = v.get("budget").and_then(Value::as_u64).unwrap_or(10);
                    out.push(SubtaskRequest { title, kind, budget });
                }
                out
            }
            None => Vec::new(),
        };
        Ok(SubmitRequest {
            goal,
            title: args.get("title").and_then(Value::as_str).map(str::to_string),
            kind: args.get("kind").and_then(Value::as_str).map(str::to_string),
            mode: args.get("mode").and_then(Value::as_str).map(str::to_string),
            acceptance: args
                .get("acceptance")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            acceptance_items: match args.get("acceptance_items") {
                Some(v) => serde_json::from_value(v.clone()).map_err(|e| {
                    Error::Invalid(format!("acceptance_items 解析失败：{e}"))
                })?,
                None => Vec::new(),
            },
            idempotency_key: args
                .get("idempotency_key")
                .and_then(Value::as_str)
                .map(str::to_string),
            steps: args.get("steps").cloned(),
            subtasks,
            observe: args.get("observe").and_then(Value::as_bool).unwrap_or(false),
            prompt: args.get("prompt").and_then(Value::as_str).map(str::to_string),
            world: args.get("world").and_then(Value::as_str).map(str::to_string),
            source: args
                .get("source")
                .and_then(Value::as_str)
                .unwrap_or("fixture")
                .to_string(),
            gmail_user: args.get("gmail_user").and_then(Value::as_str).map(str::to_string),
            gmail_name: args.get("gmail_name").and_then(Value::as_str).map(str::to_string),
            invite: args.get("invite").cloned(),
            max_model_calls: args.get("max_model_calls").and_then(Value::as_u64).unwrap_or(0),
        })
    }
}

/// What submitting a goal produced.
#[derive(Clone, Debug)]
pub struct SubmitOutcome {
    pub task_id: String,
    pub mode: ExecutionMode,
    pub duplicate: bool,
}

/// Build the router's features from the request's structure — never
/// from free text alone. A known kind means known steps; a plan
/// draft or subtasks mean multi-stage; subtasks mean parallelizable;
/// `observe` or a URL in the goal means observation-driven.
pub fn features_for(req: &SubmitRequest) -> GoalFeatures {
    let steps_known = req
        .kind
        .as_deref()
        .is_some_and(crate::commands::task_kinds::is_known_kind);
    let has_plan_draft = req.steps.is_some();
    let has_subtasks = !req.subtasks.is_empty();
    let single_observation_driven =
        req.observe || req.goal.contains("http://") || req.goal.contains("https://");
    GoalFeatures {
        has_external_side_effect: false,
        multi_stage: has_plan_draft || has_subtasks,
        needs_approval: false,
        has_dependency: false,
        has_external_submit: false,
        has_long_wait: false,
        independently_parallelizable: has_subtasks,
        steps_known,
        single_observation_driven,
        has_plan_draft,
        has_subtasks,
    }
}

/// Validate a plan draft structurally: 1–32 steps, unique ids, a
/// non-empty tool per step. Tool existence in the registry is NOT
/// checked here — the plan executor validates each step against the
/// live registry before anything runs, so a draft naming a removed
/// tool fails there with the plan still unexecuted.
pub fn parse_plan_draft(steps: &Value) -> Result<Vec<PlanStep>> {
    let raw = steps
        .as_array()
        .ok_or_else(|| Error::Invalid("steps 必须是步骤数组".into()))?;
    if raw.is_empty() || raw.len() > 32 {
        return Err(Error::Invalid(format!(
            "计划需要 1–32 步，实际 {} 步",
            raw.len()
        )));
    }
    let mut out = Vec::with_capacity(raw.len());
    let mut ids = Vec::new();
    for (i, v) in raw.iter().enumerate() {
        let step: PlanStep = serde_json::from_value(v.clone())
            .map_err(|e| Error::Invalid(format!("第 {} 个步骤解析失败：{e}", i + 1)))?;
        if step.id.trim().is_empty() || step.title.trim().is_empty() {
            return Err(Error::Invalid(format!(
                "第 {} 个步骤缺少 id 或 title",
                i + 1
            )));
        }
        if step.tool.trim().is_empty() {
            return Err(Error::Invalid(format!(
                "第 {} 个步骤（{}）缺少 tool",
                i + 1,
                step.id
            )));
        }
        if ids.contains(&step.id) {
            return Err(Error::Invalid(format!("步骤 id `{}` 重复", step.id)));
        }
        ids.push(step.id.clone());
        out.push(step);
    }
    Ok(out)
}

/// Submit a natural-language goal as a durable task: route, validate
/// the proposed mode, persist the task + its execution parameters,
/// and hand it to the resident worker. A direct-answer goal is
/// refused — the main agent should answer it inline, not file it.
pub fn submit_goal(db: &Db, req: &SubmitRequest, now: &Timestamp) -> Result<SubmitOutcome> {
    if req.goal.trim().is_empty() {
        return Err(Error::Invalid("goal 不能为空".into()));
    }
    let features = features_for(req);
    let proposed = match &req.mode {
        Some(m) => ExecutionMode::parse(m)
            .ok_or_else(|| Error::Invalid(format!("未知执行方式 {m}")))?,
        None => route_goal(&features),
    };
    validate_mode(proposed, &features, req.kind.as_deref().unwrap_or(""))
        .map_err(Error::Invalid)?;
    if proposed == ExecutionMode::DirectAnswer {
        return Err(Error::Invalid(
            "这个问题直接回答即可，不需要创建持久任务（没有外部动作、多阶段或需观察的步骤）".into(),
        ));
    }
    let kind = match proposed {
        ExecutionMode::FixedFlow => req.kind.clone().ok_or_else(|| {
            Error::Invalid("固定流程需要 kind（meeting-brief / meeting-invite / weekly-report）".into())
        })?,
        ExecutionMode::BoundedReact => "react".to_string(),
        ExecutionMode::PlanAndExecute => "plan".to_string(),
        ExecutionMode::DurableSubtasks => "delegate-join".to_string(),
        ExecutionMode::DirectAnswer => unreachable!(),
    };
    // The plan draft is validated before the task exists, so a bad
    // draft never leaves a half-created task behind.
    let draft = match &req.steps {
        Some(s) => Some(parse_plan_draft(s)?),
        None => None,
    };
    // The acceptance contract freezes with the plan: structured
    // items plus legacy string criteria as NL items, validated
    // against the draft steps now — a bad reference fails the
    // submission, not the finished task. Without a plan draft there
    // is nothing to freeze against yet (the ReAct task-version entry
    // is reserved): string criteria keep their legacy home in the
    // task context, and structured items are refused rather than
    // silently dropped.
    if !req.acceptance_items.is_empty() && draft.is_none() {
        return Err(Error::Invalid(
            "结构化验收条件需要随计划草案一起提交（无计划的 ReAct 任务暂不支持）".into(),
        ));
    }
    let mut acceptance_items = req.acceptance_items.clone();
    if let Some(steps) = &draft {
        acceptance_items.extend(nl_items(&req.acceptance));
        if !acceptance_items.is_empty() {
            validate_acceptance(&acceptance_items, steps)?;
        }
    }
    let mut body = json!({
        "kind": kind,
        "source": req.source,
        "mode": proposed.as_str(),
        "max_model_calls": req.max_model_calls,
    });
    // No title given: default from the goal (first 30 chars), so a
    // react/plan task is not filed under the brief's default title.
    let title = match &req.title {
        Some(t) => t.clone(),
        None => {
            let t: String = req.goal.chars().take(30).collect();
            t
        }
    };
    body["title"] = json!(title);
    if let Some(w) = &req.world {
        body["world"] = json!(w);
    }
    if let Some(p) = &req.prompt {
        body["prompt"] = json!(p);
    }
    // A ReAct task with no explicit prompt runs on the goal itself:
    // the default "调研：<title>" would inherit the 30-char title
    // truncation and could cut a URL the goal named in half.
    if kind == "react" && req.prompt.is_none() {
        body["prompt"] = json!(req.goal.clone());
    }
    if let Some(u) = &req.gmail_user {
        body["gmail_user"] = json!(u);
    }
    if let Some(n) = &req.gmail_name {
        body["gmail_name"] = json!(n);
    }
    if let Some(inv) = &req.invite {
        body["invite"] = inv.clone();
    }
    let params = RunParams::from_create_body(&body)?;
    let key = match &req.idempotency_key {
        Some(k) => k.clone(),
        None => {
            let digest = Sha256::digest(
                format!(
                    "{}\n{}\n{}\n{}",
                    req.goal,
                    kind,
                    proposed.as_str(),
                    serde_json::to_string(&body).unwrap_or_default()
                )
                .as_bytes(),
            );
            format!("submit:{digest:x}")
        }
    };
    let hash = params.payload_hash()?;
    let params_json = params.to_canonical_json()?;
    let store = TaskStore::new(db);
    let (task_id, duplicate) = match store.create_with_params(
        &kind,
        &params.title,
        "chat",
        "",
        3,
        Some(&key),
        &hash,
        &params_json,
        now,
    )? {
        CreateOutcome::Created(t) => (t.id, false),
        CreateOutcome::Duplicate(t) => (t.id, true),
        CreateOutcome::Conflict { existing_id } => {
            return Err(Error::Invalid(format!(
                "idempotency_key 已用于其他内容的任务 {existing_id}（同一键只能提交相同内容）"
            )));
        }
    };
    store.set_source(&task_id, &params.source, now)?;
    if params.max_model_calls > 0 {
        let _ = store.set_budget(&task_id, params.max_model_calls, now);
    }
    // The goal and its acceptance live in the task context, so the
    // plan card shows what "done" means without re-reading the chat.
    if !req.acceptance.is_empty() || !req.goal.is_empty() {
        let _ = TaskContextStore::new(db).save(
            &TaskContext {
                task_id: task_id.clone(),
                goal: req.goal.clone(),
                facts: Vec::new(),
                constraints: req.acceptance.clone(),
                evidence: Vec::new(),
                open_items: Vec::new(),
            },
            now,
        );
    }
    // Plan-and-Execute: the draft becomes the task's plan, submitted
    // for plan approval (draft = false, like the invite) — the
    // worker runs it only after the plan is approved.
    if let Some(steps) = draft {
        let plan = PlanStore::new(db).create(&params.title, &req.goal, "", steps, false, now)?;
        if !acceptance_items.is_empty() {
            PlanStore::new(db).set_acceptance(&plan.id, &acceptance_items, now)?;
        }
        store.set_plan_id(&task_id, &plan.id, now)?;
    }
    // Durable subtasks: spawn one ledger child per subtask (same
    // sequence as `task delegate`), each inheriting the parent's
    // execution parameters, so the resident worker runs them with no
    // further input and the parent joins on their results.
    for (i, sub) in req.subtasks.iter().enumerate() {
        let step_id = format!("sub-{}", i + 1);
        ensure_delegation_step(db, &task_id, &step_id, &sub.title, now)?;
        let d = DelegationStore::new(db).spawn(
            &task_id,
            &step_id,
            ChildSpec::new(&sub.kind, &sub.title, "chat", sub.budget),
            now,
        )?;
        let _ = crate::commands::task_run_params_child::inherit_params_for_child(db, &task_id, &d.child_task, &sub.kind, &sub.title, now);
    }
    Ok(SubmitOutcome { task_id, mode: proposed, duplicate })
}

