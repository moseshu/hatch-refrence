//! The deterministic (non-Agent) step runners of the M0 task worker:
//! `triage` (the triage workflow over the fixture mailbox) and
//! `artifact` (write the brief file through the `fs_write` approval
//! gate — M1: never a direct `std::fs::write` any more).

use hatch_connectors::mail::fixture::FileOutbox;
use hatch_connectors::MailSource;
use hatch_core::time::Timestamp;
use hatch_core::{CancelToken, Error, Result};
use hatch_eval::oracle::oracle_model;
use hatch_eval::World;
use hatch_llm::{Router, ToolCall};
use hatch_policy::{ApprovalQueue, ApprovalStatus, AttentionPolicy, Gatekeeper, PolicyConfig};
use hatch_store::Db;
use hatch_tasks::{EventLog, ExecStatus, Lease, TaskApprovals, TaskStore, UnitCallSpec, UnitStore};
use hatch_tools::builtin::{add_general_tools, default_registry, GeneralTools};
use hatch_tools::ws_exec::WritePayload;
use hatch_tools::{
    dispatch_call, CreateEventExecutor, ExecutorRegistry, SendEmailExecutor, ToolContext,
    Workspace, WriteFileExecutor,
};
use hatch_triage::{TriageConfig, TriageWorkflow};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Step 1: run the triage workflow over the task's mailbox. The
/// mailbox is injected (fixture world or real Gmail); the workflow
/// itself is unchanged, so desensitization applies either way.
/// Triage never sends real mail: its executor keeps the `FileOutbox`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_triage(
    db: &Db,
    world: &World,
    source: &mut dyn MailSource,
    now: &Timestamp,
    tz: &chrono_tz::Tz,
    out_dir: &Path,
    live_router: Option<&Router>,
) -> Result<hatch_triage::Receipt> {
    let account = world.persona.email.clone();
    let outbox = Arc::new(FileOutbox::new(out_dir, &account)?);
    let exec = ExecutorRegistry::new()
        .with(Arc::new(SendEmailExecutor::new(outbox.clone(), &account)))
        .with(Arc::new(CreateEventExecutor));
    let gate = Gatekeeper::new(PolicyConfig::default());
    // M1: `--live` runs triage on the real model; the default is still
    // the scripted oracle (CI makes zero real calls).
    let script_router;
    let router: &Router = match live_router {
        Some(r) => r,
        None => {
            script_router = Router::single(oracle_model(world));
            &script_router
        }
    };
    let wf = TriageWorkflow {
        db,
        router,
        gate: &gate,
        executors: &exec,
        attention: AttentionPolicy {
            tz: *tz,
            ..Default::default()
        },
        config: TriageConfig {
            user_name: world.persona.name.clone(),
            user_email: world.persona.email.clone(),
            tz: *tz,
            fetch_limit: 100,
            approval_ttl_hours: 72,
        },
    };
    wf.run(source, now)
}

/// What the artifact step resolved to on this run.
pub(crate) enum ArtifactResult {
    /// The file exists with exactly the gather output (written by the
    /// approved executor, or already there from an earlier run).
    Done(PathBuf),
    /// An approval is pending (or approved but not yet executed); the
    /// worker must block the task on it.
    AwaitingApproval(String),
    /// The recorded approval can no longer produce the file (rejected,
    /// expired, invalidated, or the file does not match the receipt);
    /// the step fails and the task fails with this note.
    Dead(String),
}

/// The origin of the artifact write (M1, versioned in task 5): one
/// value per task AND plan version. Stable across crashes, so a
/// re-dispatch after a crash between dispatch and recording finds the
/// same approval instead of queuing a duplicate — but scoped to the
/// plan version, so an edit that bumps the version (and invalidates
/// the old approval) mints a fresh idempotency key instead of
/// re-dispatching into the invalidated approval's dead end.
pub(crate) fn artifact_origin(task_id: &str, plan_version: u64) -> String {
    format!("task:{task_id}:artifact:v{plan_version}")
}

/// Step 3: write the brief artifact — through the `fs_write` approval
/// gate, never a direct `std::fs::write` (M1 fixes the M0 deviation).
/// The file version comes from the recorded approval's payload when
/// one exists, else from the count of prior `artifact_written` events,
/// so a resume never writes a second version of an already-written
/// file, and never re-dispatches an approval that is already queued.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_artifact(
    db: &Db,
    task_id: &str,
    lease: &Lease,
    units: &UnitStore,
    out_dir: &Path,
    now: &Timestamp,
    tz: &chrono_tz::Tz,
    user_email: &str,
    user_name: &str,
    // File-name stem, e.g. "brief-v" or "weekly-report-v" — the
    // versioned file is "{stem}{version}.md".
    file_stem: &str,
) -> Result<ArtifactResult> {
    let step = units
        .get_step(task_id, "artifact")?
        .ok_or_else(|| Error::NotFound("artifact 步骤不存在".into()))?;
    let gather = units
        .get_step(task_id, "gather")?
        .ok_or_else(|| Error::NotFound("gather 步骤不存在".into()))?;
    // The gather answer is in code space: a real model only ever saw
    // codes, and the scripted brief is forwarded the same way. Map
    // it back before anything is dispatched, verified or written, so
    // the approval shows and the file holds real names. Rebuilding
    // the mapping from the same database yields the same codes, so a
    // resume after a crash maps back identically.
    let mapper = hatch_triage::desensitize::mapper_for(db, user_email, user_name)?;
    let expected = mapper.reverse(&gather.output);
    // The task's current plan version scopes the origin (see
    // `artifact_origin`): a resume at the same version reuses the
    // queued approval, a resume after an edit gets a fresh one.
    let task = TaskStore::new(db)
        .get(task_id)?
        .ok_or_else(|| Error::NotFound(format!("task {task_id} 不存在")))?;
    let origin = artifact_origin(task_id, task.plan_version);
    let queue = ApprovalQueue::new(db);

    // 1. A recorded approval: resolve it — never re-dispatch.
    if !step.approval_id.is_empty() {
        let approval = match queue.get(&step.approval_id) {
            Ok(a) => a,
            Err(e) => {
                return Ok(ArtifactResult::Dead(format!(
                    "artifact 的审批 {} 找不到了：{e}",
                    step.approval_id
                )));
            }
        };
        return resolve_approval(db, task_id, lease, units, &approval, &expected, now);
    }

    // 2. Orphan approval: dispatched in a previous run, crashed before
    // the step recorded it. Reuse it if it targets this version.
    let version = artifact_version(db, task_id)? + 1;
    let rel = format!("{file_stem}{version}.md");
    if let Some(id) = find_orphan_approval(db, &origin, &rel)? {
        record_approval(db, task_id, lease, units, &id, now)?;
        return Ok(ArtifactResult::AwaitingApproval(id));
    }

    // 3. Receipt-first: the target file already exists with the exact
    // expected content (the executor ran, the worker died before
    // completing the step). Complete without dispatching.
    let path = out_dir.join(&rel);
    if verify_file(&path, &expected)? {
        return finish_artifact(
            db,
            task_id,
            lease,
            units,
            &path,
            &expected,
            "文件已存在，内容与 gather 结果一致",
            None,
            now,
        );
    }

    // 4. Dispatch `fs_write` through the gate.
    let _ = std::fs::create_dir_all(out_dir);
    let ws = Workspace::new(out_dir)?;
    let registry = add_general_tools(
        default_registry(),
        &GeneralTools {
            workspace: Some(ws.clone()),
            web_fetch: false,
            search_backend: None,
            max_command_timeout_secs: 60,
        },
    );
    let executors = ExecutorRegistry::new().with(Arc::new(WriteFileExecutor));
    let gate = Gatekeeper::new(PolicyConfig::default());
    let call = ToolCall {
        id: "artifact.write".into(),
        name: "fs_write".into(),
        arguments: json!({"path": rel, "content": expected}),
    };
    let ctx = ToolContext {
        db,
        now: *now,
        tz: *tz,
        user_email: user_email.to_string(),
        user_name: user_name.to_string(),
        origin,
        tainted: gather_tainted(units, task_id),
        call_id: call.id.clone(),
        cancel: CancelToken::new(),
    };
    let result = dispatch_call(&registry, &executors, &gate, &call, &ctx);
    // The message is for the human; the decision follows the stored
    // approval status (the message text is never parsed).
    let Some(id) = result.approval_id else {
        return Ok(ArtifactResult::Dead(format!(
            "简报写入未进入审批：{}",
            result.content.trim()
        )));
    };
    let approval = match queue.get(&id) {
        Ok(a) => a,
        Err(e) => {
            return Ok(ArtifactResult::Dead(format!(
                "简报写入审批 {id} 找不到了：{e}"
            )));
        }
    };
    match approval.status {
        ApprovalStatus::Executed => {
            resolve_approval(db, task_id, lease, units, &approval, &expected, now)
        }
        ApprovalStatus::Pending | ApprovalStatus::Approved => {
            record_approval(db, task_id, lease, units, &id, now)?;
            Ok(ArtifactResult::AwaitingApproval(id))
        }
        _ => Ok(ArtifactResult::Dead(format!(
            "简报写入审批 {} 已{}，不能再写入",
            id,
            approval.status.as_str()
        ))),
    }
}

/// Resolve one recorded approval into a step outcome. The status in
/// the queue — not the dispatch message — is authoritative.
fn resolve_approval(
    db: &Db,
    task_id: &str,
    lease: &Lease,
    units: &UnitStore,
    approval: &hatch_policy::Approval,
    expected: &str,
    now: &Timestamp,
) -> Result<ArtifactResult> {
    match approval.status {
        ApprovalStatus::Executed => {
            let payload: WritePayload = serde_json::from_value(approval.payload.clone())
                .map_err(|e| Error::Invalid(format!("简报审批 payload 解析失败：{e}")))?;
            let path = payload.root.join(&payload.path);
            finish_artifact(
                db,
                task_id,
                lease,
                units,
                &path,
                expected,
                &format!("审批 {} 已执行", approval.id),
                Some(&approval.idempotency_key),
                now,
            )
        }
        ApprovalStatus::Pending | ApprovalStatus::Approved => {
            Ok(ArtifactResult::AwaitingApproval(approval.id.clone()))
        }
        _ => Ok(ArtifactResult::Dead(format!(
            "简报写入审批 {} 已{}，不能再写入",
            approval.id,
            approval.status.as_str()
        ))),
    }
}

/// Record an approval id on the artifact step and link it to the task
/// at the task's current plan version (the version check in the
/// approval path uses this link; §10.2).
fn record_approval(
    db: &Db,
    task_id: &str,
    lease: &Lease,
    units: &UnitStore,
    approval_id: &str,
    now: &Timestamp,
) -> Result<()> {
    units.set_step_approval(task_id, lease, "artifact", approval_id, now)?;
    let task = TaskStore::new(db)
        .get(task_id)?
        .ok_or_else(|| Error::NotFound(format!("任务 {task_id} 不存在")))?;
    TaskApprovals::new(db).link(task_id, approval_id, task.plan_version, "artifact", now)?;
    Ok(())
}

/// Find a pending or approved `write_file` approval with this origin
/// whose payload targets `rel` (any other version is not usable).
fn find_orphan_approval(db: &Db, origin: &str, rel: &str) -> Result<Option<String>> {
    let queue = ApprovalQueue::new(db);
    for status in [ApprovalStatus::Pending, ApprovalStatus::Approved] {
        for a in queue.list(Some(status))? {
            if a.origin != origin || a.kind != "write_file" {
                continue;
            }
            let Ok(payload) = serde_json::from_value::<WritePayload>(a.payload) else {
                continue;
            };
            if payload.path == rel {
                return Ok(Some(a.id));
            }
        }
    }
    Ok(None)
}

/// How many artifact versions this task has already written.
fn artifact_version(db: &Db, task_id: &str) -> Result<usize> {
    let log = EventLog::new(db);
    Ok(log
        .events_since(task_id, 0, 1000)?
        .iter()
        .filter(|e| e.kind == "artifact_written")
        .count())
}

/// `true` iff the file exists with exactly the expected content. A
/// missing file is a clean `false`; a read error is a real error.
fn verify_file(path: &Path, expected: &str) -> Result<bool> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text == expected),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(Error::Invalid(format!(
            "读取简报文件 {} 失败：{e}",
            path.display()
        ))),
    }
}

/// Whether the gather results were untrusted (the dispatch context is
/// tainted). Unknown → tainted, the safe default.
fn gather_tainted(units: &UnitStore, task_id: &str) -> bool {
    let Ok(Some(u)) = units.latest_unit(task_id, "gather") else {
        return true;
    };
    let Ok(results) = units.unit_results(task_id, u.unit_seq) else {
        return true;
    };
    results.iter().any(|r| r.trust == "untrusted")
}

/// Complete the artifact step once the file is verified: write the
/// `artifact_written` event once per path, record the fs_write
/// observation on the artifact unit (reusing a unit that already
/// recorded this path, so a resume does not duplicate it), and store
/// the approval's ledger key as the step receipt.
#[allow(clippy::too_many_arguments)]
fn finish_artifact(
    db: &Db,
    task_id: &str,
    lease: &Lease,
    units: &UnitStore,
    path: &Path,
    expected: &str,
    note: &str,
    receipt: Option<&str>,
    now: &Timestamp,
) -> Result<ArtifactResult> {
    if !verify_file(path, expected)? {
        return Ok(ArtifactResult::Dead(format!(
            "简报文件 {} 缺失或内容与 gather 结果不一致（被外部修改？）",
            path.display()
        )));
    }
    let log = EventLog::new(db);
    let path_str = path.display().to_string();
    let events = log.events_since(task_id, 0, 1000)?;
    if !events
        .iter()
        .any(|e| e.kind == "artifact_written" && e.detail.contains(&path_str))
    {
        // The version is the trailing digits after the last `v` in
        // the file stem ("brief-v1", "weekly-report-v2", …) — the
        // stem itself is not needed here.
        let version = path
            .file_stem()
            .and_then(|s| {
                let s = s.to_string_lossy();
                s.rsplit_once('v').and_then(|(_, n)| {
                    (!n.is_empty() && n.chars().all(|c| c.is_ascii_digit())).then(|| n.to_string())
                })
            })
            .unwrap_or_else(|| "?".to_string());
        let _ = log.append_event(
            task_id,
            lease,
            "artifact_written",
            "artifact",
            &format!("{path_str} (v{version})"),
            now,
        );
    }
    let rel = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or(path_str.clone());
    let unit = match units.latest_unit(task_id, "artifact")? {
        Some(u) if unit_calls_path(&u, &rel) => u,
        _ => units.begin_unit(
            task_id,
            lease,
            "artifact",
            &[UnitCallSpec {
                id: "artifact.write".into(),
                tool: "fs_write".into(),
                arguments: json!({"path": rel, "content": expected}),
            }],
            now,
        )?,
    };
    let _ = units.record_result(
        task_id,
        lease,
        unit.unit_seq,
        "artifact.write",
        "fs_write",
        note,
        "system",
        now,
    );
    let _ = units.finish_unit(task_id, lease, unit.unit_seq, ExecStatus::Done, 0, 1, now);
    if let Some(key) = receipt {
        let _ = units.set_step_receipt(task_id, lease, "artifact", key, now);
    }
    Ok(ArtifactResult::Done(path.to_path_buf()))
}

/// Whether a unit's call spec already targets this relative path (a
/// resume reuses the unit instead of recording a duplicate).
fn unit_calls_path(unit: &hatch_tasks::ExecUnit, rel: &str) -> bool {
    unit.calls.iter().any(|c| {
        c.arguments
            .get("path")
            .and_then(|p| p.as_str())
            .is_some_and(|p| p == rel)
    })
}
