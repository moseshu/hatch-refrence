//! The chat tools for generic orchestration (P1): `submit_goal`
//! files a natural-language goal as a durable task, and
//! `task_status` reads the same ledger projection as `task show`.
//! Split out of `task_orchestration` (which holds the submit
//! mechanics) so neither file grows past the line-count rule.

use super::task_orchestration::{submit_goal, SubmitRequest};
use super::task_view::{detail, TaskDetail};
use hatch_core::{Error, Result};
use hatch_llm::ToolSpec;
use hatch_policy::ActionClass;
use hatch_store::Db;
use hatch_tools::{PlannedAction, Tool, ToolOutput};
use serde_json::{json, Value};

/// The `submit_goal` chat tool: file a natural-language goal as a
/// durable task and let the resident worker run it.
pub struct SubmitGoalTool;

impl Tool for SubmitGoalTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "submit_goal".into(),
            description: "Submit the user's natural-language goal as a durable task: routed by risk and task type to a fixed flow, bounded ReAct, Plan-and-Execute, or durable subtasks; written to the task ledger and run automatically by the resident worker — you do not wait for it to finish. Do not use this for simple questions; answer directly. Returns the task id and execution mode; check progress with task_status.".into(),
            parameters: json!({"type": "object", "properties": {
                "goal": {"type": "string", "description": "the natural-language goal (required), e.g. \"prepare my meeting with Li Ting tomorrow, checking decisions in recent mail\"."},
                "title": {"type": "string", "description": "optional: task title; defaults by kind when omitted."},
                "kind": {"type": "string", "description": "optional: fixed-flow kind (meeting-brief / meeting-invite / weekly-report). Without kind, routing by goal structure goes to react / plan / delegate-join."},
                "mode": {"type": "string", "description": "optional: the execution mode you propose (fixed-flow / bounded-react / plan-and-execute / durable-subtasks). The runtime validates it; a mismatch is refused with guidance on which to choose."},
                "acceptance": {"type": "array", "items": {"type": "string"}, "description": "optional: acceptance criteria, e.g. \"the brief cites the source mail for each decision\". With a plan draft these freeze into the task acceptance contract as natural-language items."},
                "acceptance_items": {"type": "array", "items": {"type": "object"}, "description": "optional: structured acceptance items frozen with the plan draft. Predicate: {kind:\"predicate\", id, left:{step, field}, op:\"equals|contains|exists|count_at_least\", right:{kind:\"field\", step, field} or {kind:\"literal\", value}, normalize?:\"rfc3339_tz|text\"}. Fields: event.title/starts_at/ends_at/location/attendees, mail.from/to/subject/body/meeting_time, file.path/content/bytes. NL: {kind:\"nl\", id, expected}."},
                "idempotency_key": {"type": "string", "description": "optional: caller idempotency key; same key + same content returns the same task, different content conflicts. Derived from the goal when omitted."},
                "steps": {"type": "array", "items": {"type": "object"}, "description": "optional: Plan-and-Execute plan draft, each step {id, title, tool, arguments, expected}."},
                "subtasks": {"type": "array", "items": {"type": "object", "properties": {"title": {"type": "string"}, "kind": {"type": "string"}, "budget": {"type": "integer"}}, "required": ["title"]}, "description": "optional: parallelizable sub-questions, each {title, kind?, budget?}."},
                "observe": {"type": "boolean", "description": "optional: true means a single question depending on live observation (web search, opening unseen pages, troubleshooting), routed to bounded ReAct. May be omitted when the goal contains a URL."},
                "prompt": {"type": "string", "description": "optional: execution prompt, overriding the default gather instruction."},
                "world": {"type": "string", "description": "optional: fixture world directory (eval/tests); omit for the gmail source."},
                "source": {"type": "string", "description": "optional: fixture (default) or gmail."},
                "gmail_user": {"type": "string", "description": "required for the gmail source: account email."},
                "gmail_name": {"type": "string", "description": "optional: gmail account display name."},
                "invite": {"type": "object", "description": "optional: meeting-invite parameters {event_title, event_start, attendee_email, location?, attendee_name?, topics?, create_event?, calendar? (local = on-device calendar by default, google = real Google Calendar)}."},
                "max_model_calls": {"type": "integer", "description": "optional: model-call budget (0 = unlimited)."}
            }, "required": ["goal"]}),
        }
    }

    fn class(&self) -> ActionClass {
        // A local ledger write: nothing leaves the machine, and the
        // task's own side effects still go through their own gates.
        ActionClass::WriteMemory
    }

    fn plan(&self, args: &Value, _ctx: &hatch_tools::ToolContext) -> Result<PlannedAction> {
        Ok(PlannedAction {
            summary: format!(
                "提交持久任务：{}",
                args.get("goal").and_then(Value::as_str).unwrap_or("")
            ),
            ..Default::default()
        })
    }

    fn call(
        &self,
        args: &Value,
        ctx: &hatch_tools::ToolContext,
    ) -> Result<ToolOutput> {
        let req = SubmitRequest::from_args(args)?;
        let out = submit_goal(ctx.db, &req, &ctx.now)?;
        Ok(ToolOutput {
            content: format!(
                "Task created: {} (mode {}, {}); the resident worker will run it automatically — check progress with task_status {}.",
                out.task_id,
                out.mode.as_str(),
                if out.duplicate { "duplicate submission, existing task returned" } else { "new task" },
                out.task_id
            ),
            trust: hatch_core::Trust::System,
            widget: None,
            image: None,
        })
    }
}

/// The `task_status` chat tool: read one task's progress from the
/// ledger — the same projection `task show` serves.
pub struct TaskStatusTool;

impl Tool for TaskStatusTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "task_status".into(),
            description: "Check how far a durable task has gotten: status, plan version, step progress (each step's title + status), pending-approval count, blocking reason. Reads the task ledger directly, not conversation summaries.".into(),
            parameters: json!({"type": "object", "properties": {
                "task_id": {"type": "string", "description": "task id (tsk_…)." }
            }, "required": ["task_id"]}),
        }
    }

    fn class(&self) -> ActionClass {
        ActionClass::ReadPersonal
    }

    fn plan(&self, args: &Value, _ctx: &hatch_tools::ToolContext) -> Result<PlannedAction> {
        Ok(PlannedAction {
            summary: format!(
                "查询任务进度：{}",
                args.get("task_id").and_then(Value::as_str).unwrap_or("")
            ),
            ..Default::default()
        })
    }

    fn call(
        &self,
        args: &Value,
        ctx: &hatch_tools::ToolContext,
    ) -> Result<ToolOutput> {
        let id = args
            .get("task_id")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Invalid("缺少 task_id".into()))?;
        Ok(ToolOutput {
            content: format_task_status(ctx.db, id)?,
            trust: hatch_core::Trust::System,
            widget: None,
            image: None,
        })
    }
}

/// One task's progress as text: status, plan version, per-step
/// progress, pending approvals, blocked reason, children.
pub fn format_task_status(db: &Db, task_id: &str) -> Result<String> {
    let d: TaskDetail = detail(db, task_id)?;
    let pending = d
        .approvals
        .iter()
        .filter(|a| a.status == "pending")
        .count();
    let mut s = format!(
        "任务 {}《{}》：{}，计划 v{}，步骤 {}/{}，待审批 {} 个",
        d.id, d.title, d.status, d.plan_version, d.steps_done, d.steps_total, pending
    );
    if !d.blocked_kind.is_empty() {
        s.push_str(&format!("，阻塞：{}", d.blocked_kind));
    }
    if !d.error.is_empty() {
        s.push_str(&format!("，错误：{}", d.error));
    }
    for step in &d.steps {
        s.push_str(&format!(
            "\n  [{}] {}（{}）{}",
            step.status,
            step.title,
            step.step_id,
            if step.output.is_empty() { String::new() } else { format!("：{}", step.output) }
        ));
    }
    if !d.children.is_empty() {
        for c in &d.children {
            s.push_str(&format!(
                "\n  子任务 {}（步骤 {}）：{} / 任务{}",
                c.child_task, c.step_id, c.status, c.child_status
            ));
        }
    }
    Ok(s)
}

