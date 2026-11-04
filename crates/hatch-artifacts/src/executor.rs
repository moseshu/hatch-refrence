//! Executors for approved artifact actions.
//!
//! Both run only through [`hatch_tools::execute_approved`]: the approval
//! queue has already checked the status and the payload hash, and the
//! idempotency ledger guarantees a re-run never builds or shares twice.
//! They hold only the artifacts root — no network, no credentials.

use crate::quota::Quota;
use crate::share::ShareRegistry;
use crate::store::{ArtifactKind, ArtifactStore};
use crate::tools::{CreateArtifactPayload, DeliverArtifactPayload, ShareArtifactPayload};
use hatch_core::{Error, Result};
use hatch_tools::{ActionExecutor, ExecContext};
use std::path::PathBuf;

/// Build the artifact version described by the approved payload.
/// The store re-scans for secrets and sanitises HTML, so a payload that
/// changed after review (or slipped past the tool) still cannot smuggle
/// a secret into a file. The store also enforces the quota, so an
/// approved build that would break it fails here (never half-written).
pub struct CreateArtifactExecutor {
    pub root: PathBuf,
    pub quota: Quota,
}

impl CreateArtifactExecutor {
    pub fn new(root: PathBuf) -> CreateArtifactExecutor {
        CreateArtifactExecutor {
            root,
            quota: Quota::default(),
        }
    }

    pub fn with_quota(mut self, quota: Quota) -> CreateArtifactExecutor {
        self.quota = quota;
        self
    }
}

impl ActionExecutor for CreateArtifactExecutor {
    fn kind(&self) -> &'static str {
        "create_artifact"
    }

    fn execute(&self, payload: &serde_json::Value, _ctx: &ExecContext) -> Result<String> {
        let p: CreateArtifactPayload = serde_json::from_value(payload.clone())?;
        let kind = ArtifactKind::parse(&p.kind)?;
        let built = ArtifactStore::new(&self.root)
            .with_quota(self.quota)
            .build(p.id.as_deref(), &p.title, kind, &p.content, &p.source)?;
        Ok(format!(
            "artifact {} v{} 已创建：{}",
            built.meta.id,
            built.manifest.version,
            built.path.display()
        ))
    }
}

/// Mark an artifact shared and mint its share token. This is the
/// only code path that sets `shared = true`; the CLI and the tool
/// both go through the approval queue to get here. The token is
/// shown exactly once, in this execution result — the registry
/// stores only its hash.
pub struct ShareArtifactExecutor {
    pub root: PathBuf,
}

impl ActionExecutor for ShareArtifactExecutor {
    fn kind(&self) -> &'static str {
        "share_artifact"
    }

    fn execute(&self, payload: &serde_json::Value, ctx: &ExecContext) -> Result<String> {
        let p: ShareArtifactPayload = serde_json::from_value(payload.clone())?;
        let store = ArtifactStore::new(&self.root);
        let meta = store.mark_shared(&p.id, ctx.approval_id)?;
        let reg = ShareRegistry::new(&self.root);
        let (share, token) = reg.mint_or_existing(&p.id, Some(ctx.approval_id))?;
        ctx.db.audit(
            "executor",
            "artifact_shared",
            &format!(
                "{} share={} approval={}",
                meta.id, share.id, ctx.approval_id
            ),
        )?;
        Ok(match token {
            Some(t) => format!(
                "artifact {}（{}）已分享（审批 {}）\n分享链接：/v1/shared/{t}\n（token 只显示这一次，请妥善保存；撤销：`hatch artifact revoke {}`）",
                meta.id, meta.title, ctx.approval_id, share.id
            ),
            None => format!(
                "artifact {}（{}）已分享（审批 {} 已有分享 {}，token 只在首次执行时显示过）",
                meta.id, meta.title, ctx.approval_id, share.id
            ),
        })
    }
}

/// Deliver the artifact described by the approved payload: the
/// single delivery entry point ([`ArtifactStore::deliver`]). The
/// returned message always contains the artifact's usable path — the
/// model must quote it to the user, not paraphrase it away. Delivery
/// requires the artifact to be shared first; an unshared id fails
/// here (the tool already refuses it at approval time, this is
/// defence in depth).
pub struct DeliverArtifactExecutor {
    pub root: PathBuf,
}

impl ActionExecutor for DeliverArtifactExecutor {
    fn kind(&self) -> &'static str {
        "deliver_artifact"
    }

    fn execute(&self, payload: &serde_json::Value, _ctx: &ExecContext) -> Result<String> {
        let p: DeliverArtifactPayload = serde_json::from_value(payload.clone())?;
        ArtifactStore::new(&self.root).deliver(&p.id)
    }
}

/// Build the `hatch artifact share` approval payload. Shared by the CLI
/// command and (via the tool) the agent; the executor above is what
/// consumes it after approval.
pub fn share_approval_payload(id: &str) -> Result<serde_json::Value> {
    if id.trim().is_empty() {
        return Err(Error::Invalid("artifact id must not be empty".into()));
    }
    Ok(serde_json::to_value(ShareArtifactPayload { id: id.to_string() })?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatch_policy::{ActionClass, ApprovalQueue, NewApproval};
    use hatch_store::Db;
    use hatch_tools::{execute_approved, ExecutorRegistry, ExecOutcome};
    use serde_json::json;
    use std::sync::Arc;

    fn setup() -> (Db, tempfile::TempDir, PathBuf, ExecutorRegistry) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("artifacts");
        let reg = ExecutorRegistry::new()
            .with(Arc::new(CreateArtifactExecutor::new(root.clone())))
            .with(Arc::new(ShareArtifactExecutor { root: root.clone() }));
        (Db::open_in_memory().unwrap(), dir, root, reg)
    }

    fn submit_share(db: &Db, id: &str) -> String {
        let now = hatch_core::time::now();
        ApprovalQueue::new(db)
            .submit(
                &NewApproval {
                    idempotency_key: format!("share-{id}"),
                    kind: "share_artifact".into(),
                    class: ActionClass::SendMessage,
                    title: format!("分享 {id}"),
                    preview: String::new(),
                    payload: json!({"id": id}),
                    reasons: vec![],
                    origin: "test".into(),
                    ttl: chrono::Duration::hours(1),
                },
                &now,
            )
            .unwrap()
            .id()
            .to_string()
    }

    #[test]
    fn share_without_approval_does_not_mark_shared() {
        let (db, _d, root, reg) = setup();
        let store = ArtifactStore::new(&root);
        let built = store.build(None, "周报", ArtifactKind::Document, "内容", "test").unwrap();
        let id = built.meta.id.clone();
        // Only submitting the approval must not share anything.
        let approval_id = submit_share(&db, &id);
        assert!(!store.get(&id).unwrap().shared);
        // Pending approval cannot execute.
        let now = hatch_core::time::now();
        assert!(execute_approved(&db, &reg, &approval_id, &now).is_err());
        assert!(!store.get(&id).unwrap().shared);
        // Approving (decide + execute) marks it shared, exactly once.
        ApprovalQueue::new(&db).decide(&approval_id, true, "", &now).unwrap();
        assert!(matches!(execute_approved(&db, &reg, &approval_id, &now).unwrap(), ExecOutcome::Executed(_)));
        let meta = store.get(&id).unwrap();
        assert!(meta.shared);
        assert_eq!(meta.shared_approval_id.as_deref(), Some(approval_id.as_str()));
    }

    #[test]
    fn share_executor_mints_token_shown_once() {
        let (db, _d, root, reg) = setup();
        let store = ArtifactStore::new(&root);
        let built = store
            .build(None, "周报", ArtifactKind::Document, "内容", "test")
            .unwrap();
        let id = built.meta.id.clone();
        let approval_id = submit_share(&db, &id);
        let now = hatch_core::time::now();
        ApprovalQueue::new(&db)
            .decide(&approval_id, true, "", &now)
            .unwrap();
        let outcome = execute_approved(&db, &reg, &approval_id, &now).unwrap();
        let msg = match outcome {
            ExecOutcome::Executed(m) => m,
            other => panic!("unexpected outcome: {other:?}"),
        };
        // The execution result is the one place the token appears.
        let token = msg
            .split("/v1/shared/")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .unwrap_or_else(|| panic!("share link with token in result: {msg}"))
            .to_string();
        assert!(store.get(&id).unwrap().shared);
        // The registry file holds only the hash, never the token.
        let raw = std::fs::read_to_string(root.join("shares.json")).unwrap();
        assert!(
            !raw.contains(&token),
            "plaintext token must never be stored"
        );
        // The token resolves; a second approval for the same artifact
        // mints a second token (different approval id).
        let (rec, _manifest, _content) = crate::share::resolve_share(&store, &token).unwrap();
        assert_eq!(rec.artifact_id, id);
        assert_eq!(rec.approval_id.as_deref(), Some(approval_id.as_str()));
    }

    #[test]
    fn create_executor_rejects_secrets_even_if_tool_did_not() {
        let (db, _d, root, reg) = setup();
        let now = hatch_core::time::now();
        let q = ApprovalQueue::new(&db);
        let id = q
            .submit(
                &NewApproval {
                    idempotency_key: "create-secret".into(),
                    kind: "create_artifact".into(),
                    class: ActionClass::WriteFile,
                    title: "x".into(),
                    preview: String::new(),
                    payload: json!({"id": null, "title": "x", "kind": "document", "content": "password = hunter22", "source": "test"}),
                    reasons: vec![],
                    origin: "test".into(),
                    ttl: chrono::Duration::hours(1),
                },
                &now,
            )
            .unwrap()
            .id()
            .to_string();
        q.decide(&id, true, "", &now).unwrap();
        assert!(matches!(execute_approved(&db, &reg, &id, &now).unwrap(), ExecOutcome::Failed(_)));
        assert!(ArtifactStore::new(&root).list().unwrap().is_empty());
    }
}
