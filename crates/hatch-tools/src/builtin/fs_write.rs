//! `fs_write` / `fs_edit`: propose a file change. The approval carries the full
//! new content plus a diff preview; the executor checkpoints the old file and
//! refuses if it changed after review.

use crate::checkpoint::{content_hash, MAX_SNAPSHOT_BYTES};
use crate::diff::unified;
use crate::tool::{arg_str, PlannedAction, Tool, ToolContext, ToolOutput};
use crate::workspace::Workspace;
use crate::ws_exec::WritePayload;
use chrono::Duration;
use hatch_core::{ids, Error, Result};
use hatch_llm::ToolSpec;
use hatch_policy::{ActionClass, DataLabel, NewApproval};
use serde_json::{json, Value};

/// Current text of `rel` (None if missing); refuses binaries and huge files.
fn current_text(ws: &Workspace, rel: &str) -> Result<Option<String>> {
    match ws.read_bytes(rel, MAX_SNAPSHOT_BYTES)? {
        None => Ok(None),
        Some(b) if b.iter().take(8192).any(|x| *x == 0) => Err(Error::Invalid(format!("{rel} 是二进制文件，不能用文本工具修改"))),
        Some(b) => String::from_utf8(b).map(Some).map_err(|_| Error::Invalid(format!("{rel} 不是 UTF-8 文本"))),
    }
}

fn normalised_rel(ws: &Workspace, rel: &str) -> Result<String> {
    let abs = ws.resolve(rel)?;
    if abs == ws.root() {
        return Err(Error::Invalid("需要文件路径，不是工作区根目录".into()));
    }
    Ok(ws.rel(&abs))
}

fn approval(ws: &Workspace, ctx: &ToolContext, rel: String, old: Option<String>, new: String, verb: &str) -> Result<NewApproval> {
    let preview = unified(&rel, old.as_deref(), &new, 3);
    let payload = WritePayload { root: ws.root().to_path_buf(), path: rel.clone(), content: new, base_hash: old.as_deref().map(|o| content_hash(o.as_bytes())) };
    let payload = serde_json::to_value(&payload)?;
    Ok(NewApproval {
        idempotency_key: ids::stable_id("write", &[&ctx.origin, &payload.to_string()]),
        kind: "write_file".into(),
        class: ActionClass::WriteFile,
        title: format!("{verb}文件 {rel}"),
        preview,
        payload,
        reasons: vec![],
        origin: ctx.origin.clone(),
        ttl: Duration::hours(24),
    })
}

fn plan_for(ws: &Workspace, rel: &str, content_probe: &str, verb: &str) -> Result<PlannedAction> {
    let rel = normalised_rel(ws, rel)?;
    let mut labels = Vec::new();
    if hatch_core::secrets::contains_secret(content_probe) {
        labels.push(DataLabel::Secret);
    }
    Ok(PlannedAction { summary: format!("{verb} {rel}"), recipients: vec![rel], labels })
}

pub struct FsWrite {
    pub ws: Workspace,
}

impl Tool for FsWrite {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fs_write".into(),
            description: "创建或整体覆盖工作区内的一个文本文件。会生成差异预览，按策略提交用户审批；写入前自动建立检查点，可回退。修改已有文件的局部内容请优先用 fs_edit。".into(),
            parameters: json!({"type": "object", "properties": {
                "path": {"type": "string"},
                "content": {"type": "string", "description": "完整的新文件内容"}
            }, "required": ["path", "content"]}),
        }
    }

    fn class(&self) -> ActionClass {
        ActionClass::WriteFile
    }

    fn plan(&self, args: &Value, _ctx: &ToolContext) -> Result<PlannedAction> {
        let content = args.get("content").and_then(Value::as_str).unwrap_or("");
        plan_for(&self.ws, arg_str(args, "path")?, content, "写入")
    }

    fn call(&self, _args: &Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        Err(Error::Invalid("fs_write only creates approvals".into()))
    }

    fn to_approval(&self, args: &Value, ctx: &ToolContext) -> Result<Option<NewApproval>> {
        let rel = normalised_rel(&self.ws, arg_str(args, "path")?)?;
        let content = args.get("content").and_then(Value::as_str).ok_or_else(|| Error::Invalid("missing string argument `content`".into()))?;
        let old = current_text(&self.ws, &rel)?;
        let verb = if old.is_some() { "覆盖" } else { "创建" };
        Ok(Some(approval(&self.ws, ctx, rel, old, content.to_string(), verb)?))
    }
}

pub struct FsEdit {
    pub ws: Workspace,
}

/// Apply an exact-text replacement; `old` must occur exactly once unless `all`.
pub fn apply_edit(text: &str, old: &str, new: &str, all: bool) -> Result<String> {
    if old.is_empty() {
        return Err(Error::Invalid("old_text 不能为空".into()));
    }
    let n = text.matches(old).count();
    match (n, all) {
        (0, _) => Err(Error::Invalid("old_text 在文件中没有找到（需要逐字匹配，包括空格和换行）".into())),
        (1, _) | (_, true) => Ok(text.replace(old, new)),
        (n, false) => Err(Error::Invalid(format!("old_text 出现了 {n} 次；请提供更多上下文使其唯一，或设置 replace_all"))),
    }
}

impl Tool for FsEdit {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fs_edit".into(),
            description: "精确替换工作区内文件的一段文字：old_text 必须在文件中逐字出现且唯一（或设置 replace_all）。生成差异预览并按策略提交审批，写入前自动建立检查点。".into(),
            parameters: json!({"type": "object", "properties": {
                "path": {"type": "string"},
                "old_text": {"type": "string"},
                "new_text": {"type": "string"},
                "replace_all": {"type": "boolean"}
            }, "required": ["path", "old_text", "new_text"]}),
        }
    }

    fn class(&self) -> ActionClass {
        ActionClass::WriteFile
    }

    fn plan(&self, args: &Value, _ctx: &ToolContext) -> Result<PlannedAction> {
        let new = args.get("new_text").and_then(Value::as_str).unwrap_or("");
        plan_for(&self.ws, arg_str(args, "path")?, new, "修改")
    }

    fn call(&self, _args: &Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        Err(Error::Invalid("fs_edit only creates approvals".into()))
    }

    fn to_approval(&self, args: &Value, ctx: &ToolContext) -> Result<Option<NewApproval>> {
        let rel = normalised_rel(&self.ws, arg_str(args, "path")?)?;
        let old_text = args.get("old_text").and_then(Value::as_str).unwrap_or("");
        let new_text = args.get("new_text").and_then(Value::as_str).ok_or_else(|| Error::Invalid("missing string argument `new_text`".into()))?;
        let all = args.get("replace_all").and_then(Value::as_bool).unwrap_or(false);
        let old = current_text(&self.ws, &rel)?.ok_or_else(|| Error::NotFound(format!("{rel} 不存在；新建文件请用 fs_write")))?;
        let new = apply_edit(&old, old_text, new_text, all)?;
        Ok(Some(approval(&self.ws, ctx, rel, Some(old), new, "修改")?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_requires_unique_exact_match() {
        assert_eq!(apply_edit("a = 1\nb = 2\n", "a = 1", "a = 3", false).unwrap(), "a = 3\nb = 2\n");
        assert!(apply_edit("x x", "x", "y", false).is_err());
        assert_eq!(apply_edit("x x", "x", "y", true).unwrap(), "y y");
        assert!(apply_edit("abc", "zzz", "y", false).is_err());
        assert!(apply_edit("abc", "", "y", false).is_err());
    }
}
