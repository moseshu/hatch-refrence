//! End-to-end: build → share → approve → deliver, plus the dispatch path
//! (widget payload through `dispatch_call`, artifact creation queued for
//! approval).

use hatch_artifacts::executor::{share_approval_payload, CreateArtifactExecutor, ShareArtifactExecutor};
use hatch_artifacts::store::{ArtifactKind, ArtifactStore};
use hatch_artifacts::tools::add_artifact_tools;
use hatch_core::CancelToken;
use hatch_llm::ToolCall;
use hatch_policy::{ActionClass, ApprovalQueue, ApprovalStatus, Gatekeeper, NewApproval, PolicyConfig};
use hatch_store::Db;
use hatch_tools::{dispatch_call, execute_approved, ExecOutcome, ExecutorRegistry, Registry, ToolContext};
use serde_json::json;
use std::sync::Arc;

fn env() -> (Db, tempfile::TempDir, std::path::PathBuf, ExecutorRegistry, Registry) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("artifacts");
    let exec = ExecutorRegistry::new()
        .with(Arc::new(CreateArtifactExecutor::new(root.clone())))
        .with(Arc::new(ShareArtifactExecutor { root: root.clone() }));
    let reg = add_artifact_tools(Registry::new(), &root);
    (Db::open_in_memory().unwrap(), dir, root, exec, reg)
}

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
fn build_share_approve_deliver_end_to_end() {
    let (db, _d, root, exec, _reg) = env();
    let store = ArtifactStore::new(&root);
    let now = hatch_core::time::now();

    // 1. Build: write-only, not shared, not delivered.
    let built = store.build(None, "周报", ArtifactKind::Document, "# 第一周\n", "e2e").unwrap();
    let id = built.meta.id.clone();
    assert!(!store.get(&id).unwrap().shared);
    assert!(!store.get(&id).unwrap().delivered);

    // 2. Share: submitting the approval must not share anything.
    let q = ApprovalQueue::new(&db);
    let approval_id = q
        .submit(
            &NewApproval {
                idempotency_key: format!("share-{id}"),
                kind: "share_artifact".into(),
                class: ActionClass::SendMessage,
                title: format!("分享 {id}"),
                preview: String::new(),
                payload: share_approval_payload(&id).unwrap(),
                reasons: vec![],
                origin: "e2e".into(),
                ttl: chrono::Duration::hours(1),
            },
            &now,
        )
        .unwrap()
        .id()
        .to_string();
    assert!(!store.get(&id).unwrap().shared, "share without approval must not mark shared");
    assert_eq!(q.get(&approval_id).unwrap().status, ApprovalStatus::Pending);

    // 3. Approve + execute: now it is shared.
    q.decide(&approval_id, true, "", &now).unwrap();
    assert!(matches!(execute_approved(&db, &exec, &approval_id, &now).unwrap(), ExecOutcome::Executed(_)));
    assert!(store.get(&id).unwrap().shared);

    // 4. Deliver: the announcement message contains the usable path.
    let message = store.deliver(&id).unwrap();
    assert!(message.contains(&built.path.display().to_string()), "delivered message must contain the link: {message}");
    assert!(message.contains("周报"));
    assert!(store.get(&id).unwrap().delivered);
}

#[test]
fn dispatch_show_options_carries_widget_and_create_queues_approval() {
    let (db, _d, _root, exec, reg) = env();
    let gate = Gatekeeper::new(PolicyConfig::default());

    // show_options: direct call, widget payload rides along, text is the fallback.
    let r = dispatch_call(
        &reg,
        &exec,
        &gate,
        &ToolCall {
            id: "c".into(),
            name: "show_options".into(),
            arguments: json!({"title": "选一个", "options": [{"id": "a", "label": "方案 A", "reply": "选 A"}]}),
        },
        &ctx(&db),
    );
    assert!(r.content.contains("方案 A") && r.content.contains("选 A"), "{}", r.content);
    let w = r.widget.expect("widget payload in dispatch result");
    assert_eq!(w["type"], "options");
    assert!(w["embed_token"].as_str().is_some_and(|t| !t.is_empty()));
    assert!(r.approval_id.is_none());

    // create_artifact: a file write, so it queues for approval and writes nothing yet.
    let r = dispatch_call(
        &reg,
        &exec,
        &gate,
        &ToolCall {
            id: "c2".into(),
            name: "create_artifact".into(),
            arguments: json!({"title": "周报", "kind": "document", "content": "# 周报\n"}),
        },
        &ctx(&db),
    );
    assert!(r.content.starts_with("已提交审批"), "{}", r.content);
    assert!(r.approval_id.is_some());
    assert!(r.widget.is_none());
}
