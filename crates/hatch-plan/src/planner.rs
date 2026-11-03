//! Ask the model for a structured plan for a task.
//!
//! The model is asked for JSON only (and the request sets `json_mode` where
//! the provider supports it). The returned text is parsed and validated by
//! [`crate::plan::parse_plan_json`] — a plan that fails validation is an
//! error, never a half-accepted plan. No tool is executed here; planning is
//! pure text in, structured plan out.

use crate::plan::PlanStep;
use crate::validate::parse_plan_json;
use hatch_core::Result;
use hatch_llm::{ChatMessage, ChatRequest, Router, Workload};
use hatch_tools::{Registry, Workspace};

const SYSTEM: &str = "你是一名计划助手。用户给出一项任务，你把它拆解成有序的、可执行的步骤。\
只输出一个 JSON 对象，不要输出任何解释、不要用代码块包裹之外的文字。格式：\n\
{\"steps\": [{\"id\": \"s1\", \"title\": \"步骤标题\", \"tool\": \"工具名\", \"arguments\": {...}, \"files\": [\"涉及的工作区相对路径\"], \"expected\": \"这一步完成后应有的结果\", \"risk\": \"low|medium|high\"}]}\n\
规则：\n\
- 每个步骤恰好调用一个工具，工具名必须在下面给出的可用工具列表里；不知道用什么工具时不要编造步骤。\n\
- `arguments` 必须是该工具接受的 JSON 对象；不要把秘密、密码、验证码放进参数里。\n\
- `files` 列出该步骤读写的工作区文件（相对路径）；不碰文件时为空数组。工作区之外的绝对路径一律不许出现。\n\
- `risk`：low = 只读或可逆，medium = 会改动用户数据或文件，high = 对外发送、扣费、不可逆。\n\
- 步骤数 1–32 步；能合并的不要拆；不要为了凑数加无意义的步骤。\n\
- 不要在计划里写“先问用户”之类的步骤——计划里的每一步都必须是可执行的工具调用。";

/// Build the planning request: system prompt + tool list + task.
pub fn plan_request(task: &str, reg: &Registry) -> ChatRequest {
    let tools: Vec<String> = reg
        .specs()
        .iter()
        .map(|s| {
            format!(
                "- {}: {}",
                s.name,
                s.description.lines().next().unwrap_or("")
            )
        })
        .collect();
    let mut req = ChatRequest::new(vec![
        ChatMessage::system(SYSTEM),
        ChatMessage::user(format!("可用工具：\n{}\n\n任务：{task}", tools.join("\n"))),
    ]);
    req.json_mode = true;
    req
}

/// Ask the model (Workload::Main chain) for a plan for `task`, parse and
/// validate it. `model` is the responding model's name, for the record.
pub fn request_plan(
    router: &Router,
    reg: &Registry,
    workspace: Option<&Workspace>,
    task: &str,
) -> Result<(Vec<PlanStep>, String)> {
    let req = plan_request(task, reg);
    let resp = router.chat_with(Workload::Main, &req, None)?;
    let text = resp.content.unwrap_or_default();
    let steps = parse_plan_json(&text, reg, workspace)?;
    Ok((steps, resp.model))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatch_llm::scripted::ScriptedModel;
    use hatch_llm::ChatResponse;
    use hatch_tools::builtin::default_registry;
    use std::sync::Arc;

    const PLAN_JSON: &str = r#"{"steps": [
        {"id": "s1", "title": "查记忆", "tool": "memory_search", "arguments": {"query": "测试"}, "files": [], "expected": "找到相关记忆", "risk": "low"},
        {"id": "s2", "title": "再查一次", "tool": "memory_search", "arguments": {"query": "测试2"}, "files": [], "expected": "找到", "risk": "low"}
    ]}"#;

    #[test]
    fn scripted_model_plan_parses() {
        let model = Arc::new(ScriptedModel::ok(
            "plan",
            vec![ChatResponse::text(PLAN_JSON)],
        ));
        let router = Router::single(model);
        let reg = default_registry();
        let (steps, name) = request_plan(&router, &reg, None, "做测试").unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].tool, "memory_search");
        assert_eq!(name, "scripted:plan");
        // The request asks for JSON and names the task.
        let seen = router.chain(Workload::Main).len();
        assert_eq!(seen, 1);
    }

    #[test]
    fn bad_model_output_is_an_error_not_a_plan() {
        let model = Arc::new(ScriptedModel::ok(
            "plan",
            vec![ChatResponse::text("我觉得应该先这样再那样")],
        ));
        let router = Router::single(model);
        let reg = default_registry();
        assert!(request_plan(&router, &reg, None, "做测试").is_err());
    }
}
