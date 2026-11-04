//! Artifact endpoints: create, get, quota, shares, the public
//! share link, revoke and delete.
//!
//! Every handler mirrors the matching CLI command: same store, same
//! validation, same defaults. Sharing goes through the approval
//! queue exactly like the CLI; revocation is real (share tokens,
//! see `hatch_artifacts::share`), and the share link itself is the
//! only unauthenticated artifact route (`GET /v1/shared/<token>`).

use super::{json_body, opt_str, req_str, ApiCtx, ApiError};
use chrono::Duration;
use hatch_artifacts::executor::share_approval_payload;
use hatch_artifacts::quota::{human_bytes, usage};
use hatch_artifacts::share::{
    delete_artifact, resolve_share, revoke_artifact_shares, ShareRegistry,
};
use hatch_artifacts::store::{ArtifactKind, ArtifactStore};
use hatch_core::Error as CoreError;
use hatch_core::{ids, time::now};
use hatch_mcp::http_proto::HttpRequest;
use hatch_policy::{ActionClass, ApprovalQueue, NewApproval};
use serde_json::{json, Value};

fn artifacts(ctx: &ApiCtx) -> ArtifactStore {
    ArtifactStore::new(&ctx.app.artifacts_dir()).with_quota(ctx.app.config.artifact_quota())
}

/// `POST /v1/artifacts` — body `{"title", "content", "kind"?,
/// "id"?}`. Content is secret-scanned before writing, like the CLI.
/// `id` appends a new version to an existing artifact. A quota
/// refusal is 403 `quota_exceeded` (the message names the limit,
/// the usage and the cap) and is audited.
pub fn artifact_create(ctx: &ApiCtx, req: &HttpRequest) -> Result<(u16, Value), ApiError> {
    let body = json_body(req)?;
    let title = req_str(&body, "title")?;
    let content = req_str(&body, "content")?;
    let kind = ArtifactKind::parse(&opt_str(&body, "kind").unwrap_or_else(|| "document".into()))?;
    let id = opt_str(&body, "id");
    let built = match artifacts(ctx).build(id.as_deref(), &title, kind, &content, "api") {
        Ok(b) => b,
        Err(e) => {
            if matches!(e, CoreError::QuotaExceeded(_)) {
                ctx.app.db.audit("api", "artifact_quota_denied", &format!("title={title} {e}"))?;
            }
            return Err(e.into());
        }
    };
    ctx.app.db.audit(
        "api",
        "artifact_built",
        &format!(
            "{} v{} {}",
            built.meta.id,
            built.manifest.version,
            built.path.display()
        ),
    )?;
    Ok((
        201,
        json!({
            "id": built.meta.id,
            "title": built.meta.title,
            "kind": kind.as_str(),
            "version": built.manifest.version,
            "created_at": built.manifest.created_at,
            "checksum": built.manifest.checksum,
            "shared": false,
            "delivered": false,
            "path": built.path.display().to_string(),
        }),
    ))
}

/// `GET /v1/artifacts/{id}?version=` — manifest plus content of
/// the latest (or requested) version.
pub fn artifact_get(ctx: &ApiCtx, req: &HttpRequest, id: &str) -> Result<(u16, Value), ApiError> {
    let version = req.query_param("version").and_then(|v| v.parse().ok());
    let store = artifacts(ctx);
    let meta = store.get(id)?;
    let (manifest, content) = store.read_version(id, version)?;
    Ok((
        200,
        json!({
            "id": meta.id,
            "title": meta.title,
            "kind": meta.kind.as_str(),
            "created_at": meta.created_at,
            "versions": meta.versions,
            "shared": meta.shared,
            "shared_approval_id": meta.shared_approval_id,
            "delivered": meta.delivered,
            "delivered_at": meta.delivered_at,
            "version": serde_json::to_value(&manifest).unwrap_or(Value::Null),
            "content": content,
        }),
    ))
}

/// `POST /v1/artifacts/{id}/share` — submits a `share_artifact`
/// approval (class `SendMessage`); the artifact is marked shared
/// only when that approval is executed. 202 when a new approval was
/// submitted, 200 when it was already shared.
pub fn artifact_share(ctx: &ApiCtx, id: &str) -> Result<(u16, Value), ApiError> {
    let store = artifacts(ctx);
    let meta = store.get(id)?;
    if meta.shared {
        return Ok((
            200,
            json!({"id": id, "shared": true, "approval_id": meta.shared_approval_id}),
        ));
    }
    let q = ApprovalQueue::new(&ctx.app.db);
    let approval = NewApproval {
        idempotency_key: ids::stable_id("share-artifact", &[id]),
        kind: "share_artifact".into(),
        class: ActionClass::SendMessage,
        title: format!("分享 artifact：{}（{}）", meta.title, id),
        preview: format!(
            "标题：{}\n类型：{}\n最新版本：v{}\n文件：{}",
            meta.title,
            meta.kind.as_str(),
            meta.latest_version(),
            meta.content_path(&ctx.app.artifacts_dir(), meta.latest_version())
                .display()
        ),
        payload: share_approval_payload(id)?,
        reasons: vec![],
        origin: "api:artifact".into(),
        ttl: Duration::hours(72),
    };
    let approval_id = q.submit(&approval, &now())?.id().to_string();
    ctx.app.db.audit(
        "api",
        "artifact_share_submitted",
        &format!("{id} approval={approval_id}"),
    )?;
    Ok((
        202,
        json!({"id": id, "shared": false, "approval_id": approval_id}),
    ))
}

/// `GET /v1/artifacts/quota` — usage against the limits (whole
/// home, single-user v1).
pub fn artifact_quota(ctx: &ApiCtx) -> Result<(u16, Value), ApiError> {
    let quota = ctx.app.config.artifact_quota();
    let used = usage(&ctx.app.artifacts_dir())?;
    Ok((
        200,
        json!({
            "count": used.count,
            "max_artifacts": quota.max_artifacts,
            "total_bytes": used.total_bytes,
            "total_bytes_human": human_bytes(used.total_bytes),
            "max_total_bytes": quota.max_total_bytes,
            "max_file_bytes": quota.max_file_bytes,
        }),
    ))
}

/// `GET /v1/artifacts/{id}/shares` — the artifact's shares (ids
/// and status only, never a token or its hash).
pub fn artifact_shares(ctx: &ApiCtx, id: &str) -> Result<(u16, Value), ApiError> {
    let store = artifacts(ctx);
    let _meta = store.get(id)?;
    let shares = ShareRegistry::new(store.root()).list_for(id)?;
    Ok((
        200,
        json!({"id": id, "shares": shares.iter().map(|s| s.to_json()).collect::<Vec<_>>()}),
    ))
}

/// `GET /v1/shared/{token}` — the share link itself: the only
/// unauthenticated artifact route (the token in the path *is* the
/// credential; the API server exempts this path from bearer auth).
/// Unknown and revoked tokens both answer 404, and the error never
/// says which — a share link must not leak whether the artifact
/// exists.
pub fn artifact_shared(ctx: &ApiCtx, token: &str) -> Result<(u16, Value), ApiError> {
    let store = artifacts(ctx);
    let (rec, manifest, content) = resolve_share(&store, token)?;
    Ok((
        200,
        json!({
            "id": rec.artifact_id,
            "share_id": rec.id,
            "title": manifest.title,
            "kind": manifest.kind.as_str(),
            "version": manifest.version,
            "created_at": manifest.created_at,
            "checksum": manifest.checksum,
            "content": content,
        }),
    ))
}

/// `POST /v1/artifacts/{id}/revoke` — revoke every active share of
/// the artifact (its share links stop working on the next request)
/// and flip `shared` off. The artifact itself stays; it cannot be
/// delivered again until re-shared. Idempotent: revoking an
/// artifact with no active shares is a 200 with `revoked: 0`.
pub fn artifact_revoke(ctx: &ApiCtx, id: &str) -> Result<(u16, Value), ApiError> {
    let store = artifacts(ctx);
    let _meta = store.get(id)?;
    let revoked = revoke_artifact_shares(&store, id)?;
    ctx.app.db.audit(
        "api",
        "artifact_shares_revoked",
        &format!("{id} revoked={revoked}"),
    )?;
    Ok((200, json!({"id": id, "shared": false, "revoked": revoked})))
}

/// `DELETE /v1/artifacts/{id}` — delete the artifact; its shares
/// are revoked first, so no share link survives the delete.
pub fn artifact_delete(ctx: &ApiCtx, id: &str) -> Result<(u16, Value), ApiError> {
    let store = artifacts(ctx);
    let _meta = store.get(id)?;
    let revoked = delete_artifact(&store, id)?;
    ctx.app.db.audit(
        "api",
        "artifact_deleted",
        &format!("{id} revoked_shares={revoked}"),
    )?;
    Ok((200, json!({"id": id, "deleted": true, "revoked_shares": revoked})))
}
