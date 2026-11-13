//! Tools that surface widgets and artifacts to the chat agent.
//!
//! * `show_options` — pure: builds an `options` widget and returns it
//!   as the tool result's widget payload. The text content is the
//!   fallback; a UI client renders the card only when the embed token is
//!   in the message. Class `ReadPersonal` (no side effect).
//! * `create_artifact` — deferred: builds a versioned artifact via the
//!   approval queue (`create_artifact` executor), class `WriteFile`,
//!   like `fs_write`. The result carries an `options` widget with the
//!   next actions (view / share / done).
//! * `share_artifact` — deferred: marks an artifact shared via the
//!   approval queue (`share_artifact` executor), class `SendMessage`
//!   because sharing is an egress. The artifact is not shared until the
//!   approval executes.
//! * `deliver_artifact` — deferred: delivers via the approval queue
//!   (`deliver_artifact` executor → `store.deliver`, the single
//!   delivery entry point), class `SendMessage`. The executor's
//!   result is the delivery message, which always contains the
//!   artifact's absolute path; an unshared artifact is refused at
//!   approval time and again in the executor.
//!
//! All four are registered by [`add_artifact_tools`]; the chat turn and
//! `hatch mcp serve` both call it, so the tools are visible to the
//! model, to plan mode (plan validation checks names against the same
//! registry) and to MCP clients via `tools/list`.

use crate::store::{ArtifactKind, ArtifactStore};
use crate::widget::{text_fallback, Widget};
use hatch_core::{ids, Error, Result};
use hatch_llm::ToolSpec;
use hatch_policy::{ActionClass, DataLabel, NewApproval};
use hatch_tools::tool::{arg_opt_str, arg_str, PlannedAction, Registry, Tool, ToolContext, ToolOutput};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Payload stored on a `create_artifact` approval.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CreateArtifactPayload {
    pub id: Option<String>,
    pub title: String,
    pub kind: String,
    pub content: String,
    pub source: String,
}

/// Payload stored on a `share_artifact` approval.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ShareArtifactPayload {
    pub id: String,
}

/// Payload stored on a `deliver_artifact` approval.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DeliverArtifactPayload {
    pub id: String,
}

/// Add the artifact/widget tools to a registry. `root` is
/// `$HATCH_HOME/artifacts`.
pub fn add_artifact_tools(reg: Registry, root: &Path) -> Registry {
    let mut reg = reg;
    for t in artifact_tools(root) {
        reg.register(t);
    }
    reg
}

/// The artifact/widget tools as a list (for callers that build their
/// own registry, e.g. the CLI app wiring).
pub fn artifact_tools(root: &Path) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ShowOptions),
        Arc::new(CreateArtifact { root: root.to_path_buf() }),
        Arc::new(ShareArtifact { root: root.to_path_buf() }),
        Arc::new(DeliverArtifact { root: root.to_path_buf() }),
    ]
}

pub struct ShowOptions;

impl Tool for ShowOptions {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "show_options".into(),
            description: "向用户展示几个选项（卡片）。返回的文本就是纯文本版；在有界面的客户端上，只有当你的回复里包含返回的 embed_token 时卡片才会激活——不含 token 时只显示文本。".into(),
            parameters: json!({"type": "object", "properties": {
                "title": {"type": "string"},
                "options": {"type": "array", "items": {"type": "object", "properties": {
                    "id": {"type": "string"}, "label": {"type": "string"}, "reply": {"type": "string"}
                }, "required": ["id", "label", "reply"]}}
            }, "required": ["title", "options"]}),
        }
    }

    fn class(&self) -> ActionClass {
        ActionClass::ReadPersonal
    }

    fn call(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let widget = Widget::from_json(&json!({
            "type": "options",
            "title": arg_str(args, "title")?,
            "options": args.get("options").cloned().unwrap_or(Value::Null),
        }))?;
        let text = text_fallback(&widget);
        Ok(ToolOutput::system(text).with_widget(widget.to_json()))
    }
}

pub struct CreateArtifact {
    pub root: PathBuf,
}

impl Tool for CreateArtifact {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "create_artifact".into(),
            description: "创建一个带版本的 artifact（文档或 HTML 卡片），保存在 $HATCH_HOME/artifacts/<id>/v<n>/。新建为 v1，传已有 id 则追加新版本，旧版本永不覆盖。内容含疑似秘密会被拒绝；HTML 会被净化。创建本身不是交付：交付要等分享获批后，用交付消息里的文件路径通知用户。".into(),
            parameters: json!({"type": "object", "properties": {
                "title": {"type": "string"},
                "kind": {"type": "string", "description": "document 或 html"},
                "content": {"type": "string"},
                "id": {"type": "string", "description": "已有 artifact 的 id（追加版本时传）；不传则新建"}
            }, "required": ["title", "kind", "content"]}),
        }
    }

    fn class(&self) -> ActionClass {
        ActionClass::WriteFile
    }

    fn plan(&self, args: &Value, _ctx: &ToolContext) -> Result<PlannedAction> {
        let mut labels = Vec::new();
        let content = args.get("content").and_then(Value::as_str).unwrap_or("");
        if hatch_core::secrets::contains_secret(content) {
            labels.push(DataLabel::Secret);
        }
        Ok(PlannedAction {
            summary: format!("创建 artifact：{}", arg_opt_str(args, "title").unwrap_or("")),
            labels,
            ..Default::default()
        })
    }

    fn call(&self, _args: &Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        Err(Error::Invalid("create_artifact only creates approvals".into()))
    }

    fn to_approval(&self, args: &Value, ctx: &ToolContext) -> Result<Option<NewApproval>> {
        let title = arg_str(args, "title")?.to_string();
        let kind = arg_str(args, "kind")?.to_string();
        ArtifactKind::parse(&kind)?;
        let content = arg_str(args, "content")?.to_string();
        let payload = CreateArtifactPayload {
            id: arg_opt_str(args, "id").map(str::to_string),
            title: title.clone(),
            kind,
            content: content.clone(),
            source: "tool:create_artifact".into(),
        };
        let payload_value = serde_json::to_value(&payload)?;
        Ok(Some(NewApproval {
            idempotency_key: ids::stable_id("artifact", &[&ctx.origin, &payload_value.to_string()]),
            kind: "create_artifact".into(),
            class: ActionClass::WriteFile,
            title: format!("创建 artifact：{title}"),
            preview: format!("标题：{title}\n内容（前 500 字）：\n{}", content.chars().take(500).collect::<String>()),
            payload: payload_value,
            reasons: vec![],
            origin: ctx.origin.clone(),
            ttl: chrono::Duration::hours(72),
        }))
    }
}

pub struct ShareArtifact {
    pub root: PathBuf,
}

impl Tool for ShareArtifact {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "share_artifact".into(),
            description: "分享一个 artifact：提交审批，批准后才标记为已分享。分享是对外动作，永远需要用户确认；未分享的 artifact 不能交付。".into(),
            parameters: json!({"type": "object", "properties": {
                "id": {"type": "string"}
            }, "required": ["id"]}),
        }
    }

    fn class(&self) -> ActionClass {
        ActionClass::SendMessage
    }

    fn plan(&self, args: &Value, _ctx: &ToolContext) -> Result<PlannedAction> {
        let id = arg_str(args, "id")?;
        Ok(PlannedAction { summary: format!("分享 artifact {id}"), ..Default::default() })
    }

    fn call(&self, _args: &Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        Err(Error::Invalid("share_artifact only creates approvals".into()))
    }

    fn to_approval(&self, args: &Value, ctx: &ToolContext) -> Result<Option<NewApproval>> {
        let id = arg_str(args, "id")?.to_string();
        let meta = ArtifactStore::new(&self.root).get(&id)?;
        let payload = ShareArtifactPayload { id: id.clone() };
        Ok(Some(NewApproval {
            idempotency_key: ids::stable_id("share-artifact", &[&id]),
            kind: "share_artifact".into(),
            class: ActionClass::SendMessage,
            title: format!("分享 artifact：{}（{}）", meta.title, id),
            preview: format!(
                "标题：{}\n类型：{}\n最新版本：v{}\n文件：{}",
                meta.title,
                meta.kind.as_str(),
                meta.latest_version(),
                meta.content_path(&self.root, meta.latest_version()).display()
            ),
            payload: serde_json::to_value(&payload)?,
            reasons: vec![],
            origin: ctx.origin.clone(),
            ttl: chrono::Duration::hours(72),
        }))
    }
}

/// Deliver an artifact: the approval-gated tool form of
/// `store.deliver`. The executor composes the announcement message —
/// the same message that names the artifact contains its usable file
/// path — and only then flips `delivered`. An unshared artifact
/// cannot be delivered; the approval executes only after the user (or
/// a grant) allowed the `SendMessage` egress.
pub struct DeliverArtifact {
    pub root: PathBuf,
}

impl Tool for DeliverArtifact {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "deliver_artifact".into(),
            description: "交付一个 artifact：提交审批，批准后执行交付，返回的交付消息里包含文件的绝对路径——把这条消息原样告诉用户，不要只说“已保存”。未分享的 artifact 不能交付（先 share_artifact 并获批）。".into(),
            parameters: json!({"type": "object", "properties": {
                "id": {"type": "string"}
            }, "required": ["id"]}),
        }
    }

    fn class(&self) -> ActionClass {
        ActionClass::SendMessage
    }

    fn plan(&self, args: &Value, _ctx: &ToolContext) -> Result<PlannedAction> {
        let id = arg_str(args, "id")?;
        Ok(PlannedAction { summary: format!("交付 artifact {id}"), ..Default::default() })
    }

    fn call(&self, _args: &Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        Err(Error::Invalid("deliver_artifact only delivers through approvals".into()))
    }

    fn to_approval(&self, args: &Value, ctx: &ToolContext) -> Result<Option<NewApproval>> {
        let id = arg_str(args, "id")?.to_string();
        let meta = ArtifactStore::new(&self.root).get(&id)?;
        if !meta.shared {
            return Err(Error::Denied(format!(
                "artifact {id} 尚未分享：先 `share_artifact` 并批准，再交付"
            )));
        }
        let payload = DeliverArtifactPayload { id: id.clone() };
        Ok(Some(NewApproval {
            idempotency_key: ids::stable_id("deliver-artifact", &[&id]),
            kind: "deliver_artifact".into(),
            class: ActionClass::SendMessage,
            title: format!("交付 artifact：{}（{}）", meta.title, id),
            preview: format!(
                "标题：{}\n类型：{}\n最新版本：v{}\n文件：{}",
                meta.title,
                meta.kind.as_str(),
                meta.latest_version(),
                meta.content_path(&self.root, meta.latest_version()).display()
            ),
            payload: serde_json::to_value(&payload)?,
            reasons: vec![],
            origin: ctx.origin.clone(),
            ttl: chrono::Duration::hours(72),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatch_tools::tool::ToolContext;
    use hatch_core::CancelToken;
    use hatch_store::Db;

    fn ctx(db: &Db) -> ToolContext<'_> {
        ToolContext {
            db,
            now: hatch_core::time::now(),
            tz: chrono_tz::Asia::Shanghai,
            user_email: "me@126.com".into(),
            user_name: String::new(),
            origin: "chat:s1".into(),
            tainted: false,
            call_id: "c1".into(),
            cancel: CancelToken::new(),
        }
    }

    #[test]
    fn show_options_returns_widget_payload_and_full_text() {
        let db = Db::open_in_memory().unwrap();
        let tool = ShowOptions;
        let out = tool
            .call(&json!({"title": "选一个", "options": [{"id": "a", "label": "方案 A", "reply": "选 A"}]}), &ctx(&db))
            .unwrap();
        assert!(out.content.contains("方案 A") && out.content.contains("选 A"));
        let w = out.widget.expect("widget payload rides with the result");
        assert_eq!(w["type"], "options");
        assert!(w["embed_token"].as_str().is_some_and(|t| !t.is_empty()));
        // The tool result text is the fallback: a client without a UI
        // shows exactly this and loses nothing.
        assert_eq!(out.content, text_fallback(&Widget::from_json(&w).unwrap()));
    }

    #[test]
    fn deliver_artifact_needs_share_first() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("artifacts");
        let db = Db::open_in_memory().unwrap();
        let store = ArtifactStore::new(&root);
        let built = store.build(None, "周报", ArtifactKind::Document, "内容", "cli").unwrap();
        let tool = DeliverArtifact { root: root.clone() };
        // Unshared: refused before any approval is queued.
        assert!(tool.to_approval(&json!({"id": built.meta.id}), &ctx(&db)).is_err());
        store.mark_shared(&built.meta.id, "appr_1").unwrap();
        let approval = tool.to_approval(&json!({"id": built.meta.id}), &ctx(&db)).unwrap().expect("approval");
        assert_eq!(approval.kind, "deliver_artifact");
        assert_eq!(approval.class, ActionClass::SendMessage);
        assert!(tool.call(&json!({}), &ctx(&db)).is_err(), "direct call is refused");
        // Unknown id fails at plan time.
        assert!(tool.to_approval(&json!({"id": "art_nope"}), &ctx(&db)).is_err());
    }

    #[test]
    fn create_artifact_defers_to_approval_and_share_needs_existing_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("artifacts");
        let db = Db::open_in_memory().unwrap();
        let tool = CreateArtifact { root: root.clone() };
        let approval = tool
            .to_approval(&json!({"title": "周报", "kind": "document", "content": "# 周报\n"}), &ctx(&db))
            .unwrap()
            .expect("approval");
        assert_eq!(approval.kind, "create_artifact");
        assert_eq!(approval.class, ActionClass::WriteFile);
        assert!(tool.call(&json!({}), &ctx(&db)).is_err(), "direct call is refused");
        // Unknown kind is rejected before any approval is queued.
        assert!(tool
            .to_approval(&json!({"title": "x", "kind": "pdf", "content": "x"}), &ctx(&db))
            .is_err());
        // Sharing a nonexistent artifact fails at plan time.
        let share = ShareArtifact { root };
        assert!(share.to_approval(&json!({"id": "art_nope"}), &ctx(&db)).is_err());
    }
}
