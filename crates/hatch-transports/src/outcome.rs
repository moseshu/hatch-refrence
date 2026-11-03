//! Reading back what a turn actually delivered.
//!
//! The agent's `deliver_artifact` tool is approval-gated: when the
//! gate auto-executes it (a grant), the executor runs
//! [`ArtifactStore::deliver`](hatch_artifacts::store::ArtifactStore::deliver)
//! and the tool result in the transcript reads `已执行（<id>）：…`.
//! [`executed_deliveries`] scans a session's transcript for
//! `deliver_artifact` tool calls whose result starts with `已执行`
//! and returns their artifact ids, newest last. The transport uses
//! this to attach the delivery message (with the absolute path) to
//! the outbound reply — the same `store.deliver` entry point, called
//! again idempotently for its message.
//!
//! The `"已执行"` prefix is `hatch-tools`' dispatch format for an
//! executed approval; the coupling is deliberate and documented here
//! rather than hidden. A queued-but-unapproved delivery is *not*
//! returned: the chat must not be told an artifact was delivered
//! before the approval ran.

use hatch_agent::SessionStore;
use hatch_core::Result;
use hatch_store::Db;

/// Artifact ids this session's turn delivered (approval executed).
pub fn executed_deliveries(db: &Db, session: &str) -> Result<Vec<String>> {
    let rows = SessionStore::new(db).transcript(session)?;
    // Map tool call id → (tool name, artifact id) for deliver calls.
    let mut calls: Vec<(String, String)> = Vec::new();
    for row in &rows {
        if row.role != "assistant" {
            continue;
        }
        for call in &row.tool_calls {
            if call.name == "deliver_artifact" {
                if let Some(id) = call.arguments.get("id").and_then(|v| v.as_str()) {
                    calls.push((call.id.clone(), id.to_string()));
                }
            }
        }
    }
    let mut out = Vec::new();
    for row in &rows {
        let Some(call_id) = row.tool_call_id.as_deref() else {
            continue;
        };
        if !row.content.starts_with("已执行") {
            continue;
        }
        if let Some((_, id)) = calls.iter().find(|(cid, _)| cid == call_id) {
            if !out.contains(id) {
                out.push(id.clone());
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatch_llm::{ChatResponse, Router, ToolCall};
    use hatch_tools::builtin::default_registry;
    use hatch_tools::{ExecutorRegistry, Registry};
    use std::sync::Arc;

    /// Build a minimal agent whose scripted model calls
    /// `deliver_artifact` and then answers. The artifact is shared
    /// beforehand; a grant auto-executes the delivery approval.
    fn agent_with_deliver(db: &Db, artifacts_root: &std::path::Path) -> (Router, Registry, ExecutorRegistry, hatch_policy::Gatekeeper) {
        let model = Arc::new(hatch_llm::scripted::ScriptedModel::ok(
            "t",
            vec![
                ChatResponse::calls(vec![ToolCall {
                    id: "c1".into(),
                    name: "deliver_artifact".into(),
                    arguments: serde_json::json!({"id": "art_test"}),
                }]),
                ChatResponse::text("已交付。"),
            ],
        ));
        let router = Router::single(model);
        let tools = hatch_artifacts::tools::add_artifact_tools(default_registry(), artifacts_root);
        let exec = ExecutorRegistry::new()
            .with(Arc::new(hatch_artifacts::executor::CreateArtifactExecutor { root: artifacts_root.to_path_buf() }))
            .with(Arc::new(hatch_artifacts::executor::ShareArtifactExecutor { root: artifacts_root.to_path_buf() }))
            .with(Arc::new(hatch_artifacts::executor::DeliverArtifactExecutor { root: artifacts_root.to_path_buf() }));
        let gate = hatch_policy::Gatekeeper::new(hatch_policy::PolicyConfig::default());
        // A grant lets the SendMessage delivery auto-execute in the turn.
        let now = hatch_core::time::now();
        hatch_policy::GrantStore::new(db)
            .add(
                &hatch_policy::NewGrant {
                    class: hatch_policy::ActionClass::SendMessage,
                    recipients: vec!["*".into()],
                    max_uses: None,
                    allow_when_tainted: false,
                    expires_at: None,
                    note: String::new(),
                },
                &now,
            )
            .unwrap();
        (router, tools, exec, gate)
    }

    #[test]
    fn executed_delivery_is_visible_in_the_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let artifacts_root = dir.path().join("artifacts");
        let db = Db::open_in_memory().unwrap();
        let store = hatch_artifacts::store::ArtifactStore::new(&artifacts_root);
        let built = store
            .build(Some("art_test"), "周报", hatch_artifacts::store::ArtifactKind::Document, "内容", "test")
            .unwrap();
        store.mark_shared(&built.meta.id, "appr_1").unwrap();
        let (router, tools, exec, gate) = agent_with_deliver(&db, &artifacts_root);
        let sessions = SessionStore::new(&db);
        let now = hatch_core::time::now();
        let session = sessions.create("transport:pipe:test", &now).unwrap();
        let agent = hatch_agent::Agent {
            db: &db,
            router: &router,
            tools: &tools,
            executors: &exec,
            gate: &gate,
            config: hatch_agent::AgentConfig::default(),
        };
        let report = agent.run_turn(&session, "把周报交付给我", &now).unwrap();
        assert!(report.answer.as_deref() == Some("已交付。"), "{report:?}");
        assert!(executed_deliveries(&db, &session).unwrap().contains(&"art_test".to_string()));
        // The transcript's tool result already carries the delivery message.
        let rows = sessions.transcript(&session).unwrap();
        assert!(rows.iter().any(|r| r.content.contains("已交付") && r.content.contains(&built.path.display().to_string())));
    }

    #[test]
    fn queued_but_unapproved_delivery_is_not_reported() {
        // Same setup, but without the grant the delivery stays queued
        // and must not be reported as delivered.
        let dir = tempfile::tempdir().unwrap();
        let artifacts_root = dir.path().join("artifacts");
        let db = Db::open_in_memory().unwrap();
        let store = hatch_artifacts::store::ArtifactStore::new(&artifacts_root);
        let built = store
            .build(Some("art_test"), "周报", hatch_artifacts::store::ArtifactKind::Document, "内容", "test")
            .unwrap();
        store.mark_shared(&built.meta.id, "appr_1").unwrap();
        let model = Arc::new(hatch_llm::scripted::ScriptedModel::ok(
            "t",
            vec![
                ChatResponse::calls(vec![ToolCall {
                    id: "c1".into(),
                    name: "deliver_artifact".into(),
                    arguments: serde_json::json!({"id": "art_test"}),
                }]),
                ChatResponse::text("已提交审批。"),
            ],
        ));
        let router = Router::single(model);
        let tools = hatch_artifacts::tools::add_artifact_tools(default_registry(), &artifacts_root);
        let exec = ExecutorRegistry::new()
            .with(Arc::new(hatch_artifacts::executor::DeliverArtifactExecutor { root: artifacts_root.to_path_buf() }));
        let gate = hatch_policy::Gatekeeper::new(hatch_policy::PolicyConfig::default());
        let sessions = SessionStore::new(&db);
        let now = hatch_core::time::now();
        let session = sessions.create("transport:pipe:test", &now).unwrap();
        let agent = hatch_agent::Agent {
            db: &db,
            router: &router,
            tools: &tools,
            executors: &exec,
            gate: &gate,
            config: hatch_agent::AgentConfig::default(),
        };
        agent.run_turn(&session, "把周报交付给我", &now).unwrap();
        assert!(executed_deliveries(&db, &session).unwrap().is_empty(), "queued is not delivered");
        assert!(!store.get("art_test").unwrap().delivered);
    }
}
