//! `hatch artifact ...` — build, list, show, share and deliver
//! versioned artifacts, plus quota and share management.
//!
//! * `build` writes a new version under
//!   `$HATCH_HOME/artifacts/<id>/v<n>/` (v1 for a new artifact, n+1 for
//!   an existing id). Source content comes from `--content` or from a
//!   `--from <path>` file, which must be inside the configured workspace
//!   (the Workspace guard); `--from` without a workspace is refused.
//!   Content is secret-scanned before writing and HTML is sanitised.
//!   The build also enforces the `[artifacts]` quota before writing
//!   anything — a refused build leaves no half-written version.
//! * `share` submits a `share_artifact` approval (class `SendMessage`);
//!   the artifact is marked shared only when that approval is executed
//!   (`hatch approvals approve <id>`), which mints a share token and
//!   prints the share link (`/v1/shared/<token>`) exactly once.
//!   Sharing is an egress, never a silent file write.
//! * `deliver` prints the announcement message — the same message that
//!   names the artifact contains its usable file path — and only then
//!   flips `delivered`. An unshared artifact cannot be delivered.
//! * `quota` shows usage against the limits; `shares` lists share
//!   tokens (ids and status, never the token itself); `revoke`
//!   invalidates a share (by share id, or all shares of an artifact);
//!   `delete` removes an artifact and revokes its shares first.
//!
//! The CLI acts on the user's explicit command, so `build` writes
//! directly (like `hatch approvals approve` executes); the agent's
//! `create_artifact` tool goes through the approval queue instead.

use crate::app::App;
use anyhow::{anyhow, bail, Result};
use clap::Subcommand;
use hatch_artifacts::executor::share_approval_payload;
use hatch_artifacts::quota::{human_bytes, usage};
use hatch_artifacts::share::{delete_artifact, revoke_artifact_shares, ShareRegistry};
use hatch_artifacts::store::{ArtifactKind, ArtifactStore};
use hatch_core::ids;
use hatch_policy::{ActionClass, ApprovalQueue, NewApproval};
use hatch_tools::Workspace;
use std::path::{Path, PathBuf};

#[derive(Subcommand)]
pub enum ArtifactCmd {
    /// 创建一个 artifact（新 id 为 v1；传 --id 追加新版本，旧版本保留）
    Build {
        /// 标题
        title: String,
        /// 类型：document 或 html
        #[arg(long, default_value = "document")]
        kind: String,
        /// 直接给内容（与 --from 二选一）
        #[arg(long)]
        content: Option<String>,
        /// 从工作区内的文件读内容
        #[arg(long)]
        from: Option<String>,
        /// 追加版本到已有 artifact
        #[arg(long)]
        id: Option<String>,
        /// 允许文件工具访问的目录（覆盖配置 [workspace] root；--from 需要）
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// 列出全部 artifact（最新版本、是否已分享/已交付）
    List,
    /// 查看一个 artifact 的 manifest 与内容（默认最新版本）
    Show {
        id: String,
        #[arg(long)]
        version: Option<u32>,
    },
    /// 提交分享审批（批准后才标记已分享，并生成分享 token）
    Share { id: String },
    /// 交付：打印含文件路径的交付消息，并标记已交付（需先分享并获批）
    Deliver { id: String },
    /// 查看配额用量与上限（总数 / 总字节 / 单个文件）
    Quota,
    /// 列出分享（可按 artifact id 过滤；只显示 id 与状态，不显示 token）
    Shares {
        /// 只列出该 artifact 的分享
        id: Option<String>,
    },
    /// 撤销分享：传分享 id（shr_…）撤销一个，或传 artifact id 撤销它的全部分享
    Revoke { id: String },
    /// 删除一个 artifact（先撤销它的全部分享，再删目录）
    Delete { id: String },
}

fn store(app: &App) -> ArtifactStore {
    ArtifactStore::new(&app.artifacts_dir()).with_quota(app.config.artifact_quota())
}

fn shares_registry(app: &App) -> ShareRegistry {
    ShareRegistry::new(&app.artifacts_dir())
}

pub fn run(home: Option<&Path>, cmd: ArtifactCmd) -> Result<()> {
    let app = App::open(home)?;
    let store = store(&app);
    let now = hatch_core::time::now();
    match cmd {
        ArtifactCmd::Build {
            title,
            kind,
            content,
            from,
            id,
            workspace,
        } => {
            let kind = ArtifactKind::parse(&kind)?;
            let (content, source) = match (content, from) {
                (Some(c), None) => (c, "cli".to_string()),
                (None, Some(rel)) => {
                    let ws = app
                        .workspace(workspace.as_deref())?
                        .ok_or_else(|| anyhow!("--from 需要配置工作区：config.toml 的 [workspace] root，或传 --workspace <目录>"))?;
                    (read_workspace_text(&ws, &rel)?, format!("workspace:{rel}"))
                }
                (Some(_), Some(_)) => bail!("--content 与 --from 只能选一个"),
                (None, None) => bail!("需要 --content 或 --from"),
            };
            match store.build(id.as_deref(), &title, kind, &content, &source) {
                Ok(built) => {
                    app.db.audit(
                        "cli",
                        "artifact_built",
                        &format!(
                            "{} v{} {}",
                            built.meta.id,
                            built.manifest.version,
                            built.path.display()
                        ),
                    )?;
                    println!(
                        "已创建 artifact {} v{}：{}\n（尚未分享、尚未交付。分享：`hatch artifact share {}`；查看：`hatch artifact show {}`）",
                        built.meta.id, built.manifest.version, built.path.display(), built.meta.id, built.meta.id
                    );
                }
                Err(e) => {
                    if matches!(e, hatch_core::Error::SecretRejected(_)) {
                        app.db.audit(
                            "cli",
                            "artifact_rejected_secret",
                            &format!("title={title} kind={}", kind.as_str()),
                        )?;
                    }
                    if matches!(e, hatch_core::Error::QuotaExceeded(_)) {
                        app.db.audit(
                            "cli",
                            "artifact_quota_denied",
                            &format!("title={title} {e}"),
                        )?;
                    }
                    return Err(anyhow!("{e}"));
                }
            }
        }
        ArtifactCmd::List => {
            let list = store.list()?;
            if list.is_empty() {
                println!("没有 artifact。创建：`hatch artifact build --content '...' --kind document '标题'`。");
            }
            for m in list {
                println!(
                    "{}  {}  {}  v{}  {}  {}",
                    m.id,
                    m.title,
                    m.kind.as_str(),
                    m.latest_version(),
                    if m.shared { "已分享" } else { "未分享" },
                    if m.delivered {
                        "已交付"
                    } else {
                        "未交付"
                    },
                );
            }
        }
        ArtifactCmd::Show { id, version } => {
            let meta = store.get(&id)?;
            let (manifest, content) = store.read_version(&id, version)?;
            println!(
                "{} — {}\n类型：{}  版本：v{}（共 {} 个版本）\n创建时间：{}\n来源：{}\n校验和：sha256:{}\n状态：{} / {}\n文件：{}",
                meta.id,
                meta.title,
                meta.kind.as_str(),
                manifest.version,
                meta.versions.len(),
                manifest.created_at,
                manifest.source,
                manifest.checksum,
                if meta.shared { "已分享" } else { "未分享" },
                if meta.delivered { "已交付" } else { "未交付" },
                meta.content_path(&app.artifacts_dir(), manifest.version).display(),
            );
            println!("\n--- 内容 ---\n{content}");
        }
        ArtifactCmd::Share { id } => {
            let meta = store.get(&id)?;
            if meta.shared {
                println!(
                    "artifact {id} 已分享（审批 {}），无需再次分享。",
                    meta.shared_approval_id.as_deref().unwrap_or("—")
                );
                return Ok(());
            }
            let q = ApprovalQueue::new(&app.db);
            let approval = NewApproval {
                idempotency_key: ids::stable_id("share-artifact", &[&id]),
                kind: "share_artifact".into(),
                class: ActionClass::SendMessage,
                title: format!("分享 artifact：{}（{}）", meta.title, id),
                preview: format!(
                    "标题：{}\n类型：{}\n最新版本：v{}\n文件：{}",
                    meta.title,
                    meta.kind.as_str(),
                    meta.latest_version(),
                    meta.content_path(&app.artifacts_dir(), meta.latest_version())
                        .display()
                ),
                payload: share_approval_payload(&id)?,
                reasons: vec![],
                origin: "cli:artifact".into(),
                ttl: chrono::Duration::hours(72),
            };
            let out = q.submit(&approval, &now)?;
            let approval_id = out.id().to_string();
            app.db.audit(
                "cli",
                "artifact_share_submitted",
                &format!("{id} approval={approval_id}"),
            )?;
            println!("已提交分享审批 {approval_id}，尚未分享。批准：`hatch approvals approve {approval_id}`；查看：`hatch approvals show {approval_id}`。");
        }
        ArtifactCmd::Deliver { id } => {
            let message = store.deliver(&id)?;
            app.db.audit("cli", "artifact_delivered", &id)?;
            println!("{message}");
        }
        ArtifactCmd::Quota => {
            let quota = app.config.artifact_quota();
            let used = usage(&app.artifacts_dir())?;
            let limit = |n: u64| {
                if n == 0 {
                    "不限".to_string()
                } else {
                    human_bytes(n)
                }
            };
            println!(
                "artifact 配额（整个 home 共用）\n总数：{} / {}\n总字节：{} / {}\n单个文件：上限 {}",
                used.count,
                if quota.max_artifacts == 0 { "不限".to_string() } else { quota.max_artifacts.to_string() },
                human_bytes(used.total_bytes),
                limit(quota.max_total_bytes),
                limit(quota.max_file_bytes),
            );
            println!("（配置：config.toml 的 [artifacts]；环境变量 HATCH_ARTIFACT_MAX_COUNT / HATCH_ARTIFACT_MAX_TOTAL_BYTES / HATCH_ARTIFACT_MAX_FILE_BYTES 覆盖文件；0 = 不限）");
        }
        ArtifactCmd::Shares { id } => {
            let list = match id {
                Some(a) => shares_registry(&app).list_for(&a)?,
                None => shares_registry(&app).list()?,
            };
            if list.is_empty() {
                println!("没有分享。分享：`hatch artifact share <id>`，批准后生成分享链接。");
            }
            for s in list {
                println!(
                    "{}  artifact={}  创建：{}  {}",
                    s.id,
                    s.artifact_id,
                    s.created_at,
                    if s.revoked {
                        format!("已撤销{}", s.revoked_at.as_deref().unwrap_or(""))
                    } else {
                        "有效".to_string()
                    },
                );
            }
        }
        ArtifactCmd::Revoke { id } => {
            if id.starts_with("shr_") {
                match hatch_artifacts::share::revoke_share(&store, &id)? {
                    Some(rec) => {
                        app.db.audit(
                            "cli",
                            "artifact_shares_revoked",
                            &format!("share={id} artifact={}", rec.artifact_id),
                        )?;
                        println!(
                            "已撤销分享 {id}（artifact {}）。该分享链接立即失效。",
                            rec.artifact_id
                        );
                    }
                    None => {
                        bail!("没有这个分享 {id}（可能已被撤销；查看：`hatch artifact shares`）")
                    }
                }
            } else {
                let meta = store.get(&id)?;
                let n = revoke_artifact_shares(&store, &id)?;
                app.db.audit(
                    "cli",
                    "artifact_shares_revoked",
                    &format!("{id} revoked={n}"),
                )?;
                if n == 0 {
                    println!("artifact {}（{}）没有有效分享可撤销。", id, meta.title);
                } else {
                    println!(
                        "已撤销 artifact {}（{}）的 {n} 个分享，分享链接立即失效。该 artifact 已不再是分享状态，未分享不能交付。",
                        id, meta.title
                    );
                }
            }
        }
        ArtifactCmd::Delete { id } => {
            let meta = store.get(&id)?;
            let revoked = delete_artifact(&store, &id)?;
            app.db.audit(
                "cli",
                "artifact_deleted",
                &format!("{id} revoked_shares={revoked}"),
            )?;
            println!(
                "已删除 artifact {}（{}），同时撤销了 {revoked} 个分享。",
                id, meta.title
            );
        }
    }
    Ok(())
}

/// Read a workspace file as UTF-8 text (size-capped, no binaries).
fn read_workspace_text(ws: &Workspace, rel: &str) -> Result<String> {
    let bytes = ws
        .read_bytes(rel, 1024 * 1024)?
        .ok_or_else(|| anyhow!("工作区内没有文件 {rel}"))?;
    if bytes.iter().take(8192).any(|x| *x == 0) {
        bail!("{rel} 是二进制文件，不能作为 artifact 内容");
    }
    String::from_utf8(bytes).map_err(|_| anyhow!("{rel} 不是 UTF-8 文本"))
}
