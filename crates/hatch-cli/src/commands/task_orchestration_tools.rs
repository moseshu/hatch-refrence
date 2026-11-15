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
            description: "把用户的自然语言目标提交为持久任务：按风险和任务类型路由到固定流程、有界 ReAct、Plan-and-Execute 或持久子任务，写入任务账本后由常驻 worker 自动执行，你不用等它跑完。简单问答不要用这个工具，直接回答。返回任务 id 和执行方式；用 task_status 查进度。".into(),
            parameters: json!({"type": "object", "properties": {
                "goal": {"type": "string", "description": "自然语言目标（必填），例如“给我准备明天和李婷的会，看看最近邮件里的决定”。"},
                "title": {"type": "string", "description": "可选：任务标题；不给则按 kind 默认。"},
                "kind": {"type": "string", "description": "可选：固定流程 kind（meeting-brief / meeting-invite / weekly-report）。不给 kind 时按目标结构路由到 react / plan / delegate-join。"},
                "mode": {"type": "string", "description": "可选：你提议的执行方式（fixed-flow / bounded-react / plan-and-execute / durable-subtasks）。运行时会校验，不符则拒绝并说明应选哪种。"},
                "acceptance": {"type": "array", "items": {"type": "string"}, "description": "可选：验收条件列表，例如“简报引用每条决定的来源邮件”。"},
                "idempotency_key": {"type": "string", "description": "可选：调用方幂等键；同一键 + 相同内容返回同一任务，不同内容返回冲突。不给则按目标内容派生。"},
                "steps": {"type": "array", "items": {"type": "object"}, "description": "可选：Plan-and-Execute 的计划草案，每步 {id, title, tool, arguments, expected}。"},
                "subtasks": {"type": "array", "items": {"type": "object", "properties": {"title": {"type": "string"}, "kind": {"type": "string"}, "budget": {"type": "integer"}}, "required": ["title"]}, "description": "可选：可并行的子问题列表，每个 {title, kind?, budget?}。"},
                "observe": {"type": "boolean", "description": "可选：true 表示单一问题依赖即时观察（搜网页、打开未见过的页面、排查问题），走有界 ReAct。目标里含网址时也可不传。"},
                "prompt": {"type": "string", "description": "可选：执行提示，覆盖默认的 gather 指令。"},
                "world": {"type": "string", "description": "可选：fixture 世界目录（评估/测试用）；gmail 来源不传。"},
                "source": {"type": "string", "description": "可选：fixture（默认）或 gmail。"},
                "gmail_user": {"type": "string", "description": "gmail 来源必填：账号邮箱。"},
                "gmail_name": {"type": "string", "description": "可选：gmail 账号显示名。"},
                "invite": {"type": "object", "description": "可选：会议邀请参数 {event_title, event_start, attendee_email, location?, attendee_name?, topics?, create_event?}。"},
                "max_model_calls": {"type": "integer", "description": "可选：模型调用预算（0 = 不限）。"}
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
                "任务已创建：{}（执行方式 {}，{}）；常驻 worker 将自动运行，用 task_status {} 查询进度。",
                out.task_id,
                out.mode.as_str(),
                if out.duplicate { "重复提交，返回已有任务" } else { "新任务" },
                out.task_id
            ),
            trust: hatch_core::Trust::System,
            widget: None,
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
            description: "查一个持久任务做到哪了：状态、计划版本、步骤进度（每步标题 + 状态）、待审批数量、阻塞原因。直接读任务账本，不读对话摘要。".into(),
            parameters: json!({"type": "object", "properties": {
                "task_id": {"type": "string", "description": "任务 id（tsk_…）。"}
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

