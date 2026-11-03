//! Parse and validate model-produced plans, and detect deviations.
//!
//! See [`crate::plan`] for the types. [`parse_plan_json`] extracts JSON
//! from model output (raw, fenced or prose-wrapped) and rejects malformed
//! plans, unknown tools, duplicate step ids and workspace escapes.
//! [`detect_deviation`] compares a current step list against the approved
//! snapshot: changed tool / arguments / files, extra steps, missing steps.

use crate::plan::{Deviation, PlanStep, MAX_STEPS};
use hatch_core::{Error, Result};
use hatch_tools::{Registry, Workspace};
use serde_json::Value;

/// Validate one step against the tool registry and the workspace guard.
/// `workspace` is `None` when no workspace is configured — then any step
/// that touches files is rejected (file tools are off without one).
pub fn validate_step(step: &PlanStep, reg: &Registry, workspace: Option<&Workspace>) -> Result<()> {
    if step.id.trim().is_empty() {
        return Err(Error::Invalid("plan step needs a non-empty `id`".into()));
    }
    if step.title.trim().is_empty() {
        return Err(Error::Invalid(format!(
            "step {} needs a non-empty `title`",
            step.id
        )));
    }
    if step.expected.trim().is_empty() {
        return Err(Error::Invalid(format!(
            "step {} needs a non-empty `expected` outcome",
            step.id
        )));
    }
    let Some(_tool) = reg.get(&step.tool) else {
        return Err(Error::Invalid(format!(
            "step {} names unknown tool `{}`; available: {}",
            step.id,
            step.tool,
            reg.names().join(", ")
        )));
    };
    if !step.arguments.is_object() {
        return Err(Error::Invalid(format!(
            "step {}: `arguments` must be a JSON object",
            step.id
        )));
    }
    // A `path` argument is a file the tool will touch even when `files`
    // does not list it — check it the same way.
    let mut paths: Vec<&str> = step.files.iter().map(String::as_str).collect();
    if let Some(p) = step.arguments.get("path").and_then(Value::as_str) {
        paths.push(p);
    }
    match workspace {
        Some(ws) => {
            for p in paths {
                ws.resolve(p)
                    .map_err(|e| Error::Invalid(format!("step {}: {e}", step.id)))?;
            }
        }
        None if !paths.is_empty() => {
            return Err(Error::Invalid(format!(
                "step {} touches files ({}) but no workspace is configured",
                step.id,
                paths.join(", ")
            )));
        }
        None => {}
    }
    Ok(())
}

/// Parse model output (raw JSON, fenced block or prose around JSON) into
/// validated steps for `task`. Returns the steps; the caller persists them.
pub fn parse_plan_json(
    text: &str,
    reg: &Registry,
    workspace: Option<&Workspace>,
) -> Result<Vec<PlanStep>> {
    let value = hatch_llm::json_extract::extract_json(text).ok_or_else(|| {
        Error::Invalid("模型输出中没有找到 JSON 计划（需要 {\"steps\": [...]}）".to_string())
    })?;
    let steps_value = value
        .get("steps")
        .ok_or_else(|| Error::Invalid("计划 JSON 缺少 `steps` 数组".to_string()))?;
    let raw_steps = steps_value
        .as_array()
        .ok_or_else(|| Error::Invalid("`steps` 必须是数组".to_string()))?;
    if raw_steps.is_empty() {
        return Err(Error::Invalid("计划至少需要一个步骤".into()));
    }
    if raw_steps.len() > MAX_STEPS {
        return Err(Error::Invalid(format!(
            "计划最多 {} 步，模型给了 {} 步",
            MAX_STEPS,
            raw_steps.len()
        )));
    }
    let mut steps = Vec::with_capacity(raw_steps.len());
    let mut seen_ids = Vec::new();
    for (i, raw) in raw_steps.iter().enumerate() {
        let step: PlanStep = serde_json::from_value(raw.clone())
            .map_err(|e| Error::Invalid(format!("第 {} 个步骤解析失败：{e}", i + 1)))?;
        if seen_ids.contains(&step.id) {
            return Err(Error::Invalid(format!("步骤 id `{}` 重复", step.id)));
        }
        seen_ids.push(step.id.clone());
        validate_step(&step, reg, workspace)?;
        steps.push(step);
    }
    Ok(steps)
}

/// Does `current` still match the approved step, field by field?
/// Returns the first mismatch as a [`Deviation`].
pub fn check_step(approved: &PlanStep, current: &PlanStep) -> Option<Deviation> {
    if approved.tool != current.tool {
        return Some(Deviation::ChangedStep {
            step_id: current.id.clone(),
            detail: format!("工具由 `{}` 改为 `{}`", approved.tool, current.tool),
        });
    }
    if approved.arguments != current.arguments {
        return Some(Deviation::ChangedStep {
            step_id: current.id.clone(),
            detail: "参数变了".into(),
        });
    }
    if approved.files != current.files {
        return Some(Deviation::ChangedStep {
            step_id: current.id.clone(),
            detail: format!("涉及文件由 {:?} 改为 {:?}", approved.files, current.files),
        });
    }
    None
}

/// Compare a whole current step list against the approved snapshot.
/// Checks order, count and every step's tool / arguments / files.
pub fn detect_deviation(approved: &[PlanStep], current: &[PlanStep]) -> Option<Deviation> {
    for (i, step) in current.iter().enumerate() {
        match approved.get(i) {
            None => {
                return Some(Deviation::ExtraStep {
                    step_id: step.id.clone(),
                })
            }
            Some(a) => {
                if a.id != step.id {
                    // Same position, different id: treat as extra + missing.
                    return Some(Deviation::ExtraStep {
                        step_id: step.id.clone(),
                    });
                }
                if let Some(d) = check_step(a, step) {
                    return Some(d);
                }
            }
        }
    }
    if let Some(a) = approved.get(current.len()) {
        return Some(Deviation::MissingStep {
            step_id: a.id.clone(),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{PlanStep, RiskLevel, StepStatus};
    use hatch_tools::builtin::default_registry;
    use serde_json::json;

    fn reg() -> Registry {
        default_registry()
    }

    fn ws() -> (tempfile::TempDir, Workspace) {
        let d = tempfile::tempdir().unwrap();
        let w = Workspace::new(d.path()).unwrap();
        (d, w)
    }

    fn step(id: &str, tool: &str, args: Value) -> PlanStep {
        PlanStep {
            id: id.into(),
            title: "t".into(),
            tool: tool.into(),
            arguments: args,
            files: vec![],
            expected: "ok".into(),
            risk: RiskLevel::Low,
            status: StepStatus::Pending,
            output: String::new(),
            approval_id: None,
        }
    }

    #[test]
    fn parses_plain_and_fenced_json() {
        let reg = reg();
        let (_d, w) = ws();
        let text = "好的，计划如下：\n```json\n{\"steps\": [{\"id\": \"s1\", \"title\": \"查记忆\", \"tool\": \"memory_search\", \"arguments\": {\"query\": \"x\"}, \"files\": [], \"expected\": \"找到\", \"risk\": \"low\"}]}\n```";
        let steps = parse_plan_json(text, &reg, Some(&w)).unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].tool, "memory_search");
    }

    #[test]
    fn rejects_malformed_json() {
        let reg = reg();
        assert!(parse_plan_json("no json at all", &reg, None).is_err());
        assert!(parse_plan_json("{\"steps\": []}", &reg, None).is_err());
        assert!(parse_plan_json("{\"steps\": \"nope\"}", &reg, None).is_err());
        assert!(parse_plan_json("{\"nope\": []}", &reg, None).is_err());
    }

    #[test]
    fn rejects_unknown_tool_and_duplicate_ids() {
        let reg = reg();
        let bad_tool = "{\"steps\": [{\"id\": \"s1\", \"title\": \"t\", \"tool\": \"rm_rf\", \"arguments\": {}, \"expected\": \"x\"}]}";
        assert!(parse_plan_json(bad_tool, &reg, None)
            .unwrap_err()
            .to_string()
            .contains("unknown tool"));
        let dup = "{\"steps\": [{\"id\": \"s1\", \"title\": \"t\", \"tool\": \"memory_search\", \"arguments\": {}, \"expected\": \"x\"}, {\"id\": \"s1\", \"title\": \"t\", \"tool\": \"memory_search\", \"arguments\": {}, \"expected\": \"x\"}]}";
        assert!(parse_plan_json(dup, &reg, None)
            .unwrap_err()
            .to_string()
            .contains("重复"));
    }

    #[test]
    fn rejects_path_escape_and_missing_workspace() {
        let (_d, w) = ws();
        let reg = hatch_tools::builtin::add_general_tools(
            default_registry(),
            &hatch_tools::builtin::GeneralTools {
                workspace: Some(w.clone()),
                web_fetch: false,
                search_backend: None,
                max_command_timeout_secs: 60,
            },
        );
        let esc = "{\"steps\": [{\"id\": \"s1\", \"title\": \"t\", \"tool\": \"fs_read\", \"arguments\": {\"path\": \"../secret\"}, \"files\": [\"../secret\"], \"expected\": \"x\"}]}";
        assert!(parse_plan_json(esc, &reg, Some(&w)).is_err());
        // A `path` argument alone is checked too, even if `files` is empty.
        let esc2 = "{\"steps\": [{\"id\": \"s1\", \"title\": \"t\", \"tool\": \"fs_read\", \"arguments\": {\"path\": \"/etc/passwd\"}, \"expected\": \"x\"}]}";
        assert!(parse_plan_json(esc2, &reg, Some(&w)).is_err());
        // No workspace at all: any file touch is rejected.
        let f = "{\"steps\": [{\"id\": \"s1\", \"title\": \"t\", \"tool\": \"fs_read\", \"arguments\": {\"path\": \"a.txt\"}, \"expected\": \"x\"}]}";
        assert!(parse_plan_json(f, &reg, None).is_err());
        // …but a file-free step is fine without a workspace.
        let ok = "{\"steps\": [{\"id\": \"s1\", \"title\": \"t\", \"tool\": \"memory_search\", \"arguments\": {\"query\": \"x\"}, \"expected\": \"x\"}]}";
        assert!(parse_plan_json(ok, &reg, None).is_ok());
        // A path inside the workspace passes.
        std::fs::write(w.root().join("a.txt"), "hi").unwrap();
        let inside = "{\"steps\": [{\"id\": \"s1\", \"title\": \"t\", \"tool\": \"fs_read\", \"arguments\": {\"path\": \"a.txt\"}, \"files\": [\"a.txt\"], \"expected\": \"x\"}]}";
        assert!(parse_plan_json(inside, &reg, Some(&w)).is_ok());
    }

    #[test]
    fn deviation_detection_covers_all_cases() {
        let a = vec![
            step("s1", "memory_search", json!({"query": "x"})),
            step("s2", "memory_search", json!({"query": "y"})),
        ];
        assert!(detect_deviation(&a, &a).is_none());
        // Changed tool.
        let mut changed = a.clone();
        changed[0].tool = "web_search".into();
        assert!(matches!(
            detect_deviation(&a, &changed),
            Some(Deviation::ChangedStep { .. })
        ));
        // Changed arguments.
        let mut changed = a.clone();
        changed[1].arguments = json!({"query": "z"});
        assert!(matches!(
            detect_deviation(&a, &changed),
            Some(Deviation::ChangedStep { .. })
        ));
        // Changed files.
        let mut changed = a.clone();
        changed[0].files = vec!["evil.txt".into()];
        assert!(matches!(
            detect_deviation(&a, &changed),
            Some(Deviation::ChangedStep { .. })
        ));
        // Extra step.
        let mut extra = a.clone();
        extra.push(step("s3", "memory_search", json!({"query": "q"})));
        assert!(matches!(
            detect_deviation(&a, &extra),
            Some(Deviation::ExtraStep { .. })
        ));
        // Skipped step.
        let missing = vec![a[0].clone()];
        assert!(matches!(
            detect_deviation(&a, &missing),
            Some(Deviation::MissingStep { .. })
        ));
        // Reordered steps: position 0 now holds s2.
        let reordered = vec![a[1].clone(), a[0].clone()];
        assert!(
            detect_deviation(&a, &reordered).is_some(),
            "reordering is a deviation"
        );
    }
}
