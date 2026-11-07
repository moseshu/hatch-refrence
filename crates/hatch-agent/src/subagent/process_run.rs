//! The child-process path for one delegation: build the [`ProcessTask`],
//! spawn and monitor the child, then replay its approvals and grants
//! into the shared database and build the handoff.

use super::process::{ApprovalWire, ConsumedGrant, GrantWire, ProcessOutcome, ProcessTask};
use super::registry::{ChildRegistry, RegistryEntry};
use super::retry::{spawn_child, SpawnFailure};
use super::run::{handoff, StartedRun};
use super::spawn;
use super::store::{RunResult, RunStatus, RunStore};
use super::types::AgentType;
use super::{ChildOutcome, Delegation, ParentCtx, TaskSpec};
use crate::agent::Agent;
use crate::progress::{clip, Observer, Progress, ProgressKind};
use crate::report::{StopReason, TurnReport};
use hatch_core::time::{parse_rfc3339, to_rfc3339, Timestamp};
use hatch_core::CancelToken;
use hatch_llm::Usage;
use hatch_memory::MemoryStore;
use hatch_policy::{ActionClass, ApprovalQueue, GrantStore, NewApproval};
use hatch_store::Db;
use std::path::PathBuf;
use std::sync::Arc;

/// Browser tools need the shared database's session records and a Chromium
/// profile owned by this process, so a child process cannot offer them.
const BROWSER_TOOLS: &[&str] = &[
    "browser_open",
    "browser_read",
    "browser_find",
    "browser_scroll",
    "browser_screenshot",
    "browser_back",
    "browser_close",
];

/// Memory lines the parent hands to a child process: profile claims plus
/// a recall over the task brief. The child has an empty local bank, so
/// this is the only memory it sees.
fn memory_context(parent: &Agent, task: &str, now: &Timestamp) -> Vec<String> {
    let mem = MemoryStore::new(parent.db);
    let profile = mem.with_tags(&["preference", "profile", "style"], now).unwrap_or_default();
    let mut out: Vec<String> = profile.iter().take(12).map(|c| format!("[{}] {}", c.id, c.text)).collect();
    out.extend(crate::agent::recall(&mem, task, &profile, now));
    out
}

/// Standing grants active in the parent, for the child to seed into its
/// own database (it cannot see the shared one).
fn grant_wires(parent: &Agent, now: &Timestamp) -> Vec<GrantWire> {
    let Ok(grants) = GrantStore::new(parent.db).list() else { return vec![] };
    grants
        .into_iter()
        .filter(|g| g.is_active(now))
        .map(|g| GrantWire {
            class: g.class.as_str().to_string(),
            recipients: g.recipients,
            max_uses: g.max_uses,
            allow_when_tainted: g.allow_when_tainted,
            expires_at: g.expires_at.map(|e| to_rfc3339(&e)),
            note: g.note,
        })
        .collect()
}

/// Replay the approvals a child process queued (or auto-executed) into the
/// shared queue, under the same idempotency keys — so the parent-side ids
/// match the child's — and return the parent-side ids.
fn replay_approvals(db: &Db, wires: &[ApprovalWire], now: &Timestamp) -> Vec<String> {
    let q = ApprovalQueue::new(db);
    let mut ids = Vec::new();
    for w in wires {
        let Ok(class) = ActionClass::parse(&w.class) else {
            let _ = db.audit("subagent", "approval_replay_failed", &format!("{}: 未知动作类别 {}", w.idempotency_key, w.class));
            continue;
        };
        let ttl = match (parse_rfc3339(&w.created_at), parse_rfc3339(&w.expires_at)) {
            (Ok(created), Ok(exp)) => exp - created,
            _ => chrono::Duration::hours(24),
        };
        let na = NewApproval {
            idempotency_key: w.idempotency_key.clone(),
            kind: w.kind.clone(),
            class,
            title: w.title.clone(),
            preview: w.preview.clone(),
            payload: w.payload.clone(),
            reasons: w.reasons.clone(),
            origin: w.origin.clone(),
            ttl,
        };
        match q.submit(&na, now) {
            Ok(out) => {
                let id = out.id().to_string();
                if w.status == "executed" {
                    // The effect already happened in the child; record it
                    // so the same approval cannot execute a second time.
                    let _ = q.decide(&id, true, "子代理按授权自动执行", now);
                    let _ = q.mark_executed(&id, "子代理已执行（结果见子代理过程摘录）", now);
                }
                ids.push(id);
            }
            Err(e) => {
                let _ = db.audit("subagent", "approval_replay_failed", &format!("{}: {e}", w.idempotency_key));
            }
        }
    }
    ids
}

/// Consume, in the shared database, the grants a child process used
/// (matched by content — child-side grant ids are random). Best-effort:
/// a failure here only means the parent's counters lag, never a crash.
fn replay_grants(db: &Db, consumed: &[ConsumedGrant], now: &Timestamp) {
    let Ok(grants) = GrantStore::new(db).list() else { return };
    let store = GrantStore::new(db);
    for c in consumed {
        for _ in 0..c.uses.max(0) {
            if let Some(g) = grants
                .iter()
                .find(|g| g.class.as_str() == c.class && g.recipients == c.recipients && g.note == c.note && g.is_active(now))
            {
                let _ = store.consume(&g.id);
            }
        }
    }
}

/// Everything a child process needs, plus the binary to spawn.
/// Factored out of [`run_process`] so the async path can build the
/// same task and spawn it without waiting.
#[allow(clippy::too_many_arguments)]
fn build_task(
    parent: &Agent,
    d: &Delegation,
    spec: &TaskSpec,
    pc: &ParentCtx,
    run_id: &str,
    ty: &AgentType,
    top: &str,
    parent_ref: &str,
) -> Result<(ProcessTask, PathBuf), String> {
    let child_depth = d.depth + 1;
    let names = parent.tools.names();
    let allowed = ty.allowed(&names);
    let (tools, dropped_browser): (Vec<String>, bool) = {
        let mut kept = Vec::new();
        let mut dropped = false;
        for t in allowed {
            if BROWSER_TOOLS.contains(&t) {
                dropped = true;
            } else {
                kept.push(t.to_string());
            }
        }
        (kept, dropped)
    };
    let mut instructions = ty.instructions.clone();
    if dropped_browser {
        instructions.push_str("\n注意：本次运行在独立进程中，浏览器工具不可用，不要尝试调用它们。");
    }
    let max_steps = spec.max_steps.unwrap_or(ty.max_steps).clamp(1, ty.max_steps);
    let scratch = std::env::temp_dir().join(format!("hatch-sa-{run_id}"));
    let binary = match &d.binary {
        Some(b) => b.clone(),
        None => std::env::current_exe().map_err(|e| format!("无法确定当前可执行文件：{e}"))?,
    };
    let task = ProcessTask {
        run_id: run_id.to_string(),
        agent_type: ty.name.clone(),
        task: spec.task.clone(),
        instructions,
        max_steps,
        depth: child_depth,
        max_depth: d.max_depth,
        max_parallel: d.max_parallel,
        max_tasks: d.max_tasks,
        session: top.to_string(),
        parent_ref: parent_ref.to_string(),
        tainted: pc.tainted,
        origin: format!("subagent:{run_id}"),
        user_name: parent.config.user_name.clone(),
        user_email: parent.config.user_email.clone(),
        tz: parent.config.tz.name().to_string(),
        workload: ty.workload.as_str().to_string(),
        tools,
        memory_context: memory_context(parent, &spec.task, pc.now),
        skills: parent.config.skills.clone(),
        home: d.home.clone(),
        workspace: d.workspace.clone(),
        scratch_dir: scratch,
        timeout_secs: d.timeout_secs,
        grants: grant_wires(parent, pc.now),
    };
    Ok((task, binary))
}

/// Run one delegated task in a child process. Returns `Err` only when the
/// child could not be spawned at all (the caller falls back to in-process).
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_process(
    parent: &Agent,
    d: &Delegation,
    spec: &TaskSpec,
    pc: &ParentCtx,
    store: &RunStore,
    run_id: &str,
    ty: &AgentType,
    top: &str,
    parent_ref: &str,
) -> Result<ChildOutcome, SpawnFailure> {
    let child_depth = d.depth + 1;
    let (task, binary) = build_task(parent, d, spec, pc, run_id, ty, top, parent_ref)
        .map_err(|m| SpawnFailure::permanent(m, "配置错误"))?;
    // The child's transcript lives in its own (deleted) database; record
    // the run under the `subagent:<id>` label.
    let _ = store.start(run_id, &format!("subagent:{run_id}"));
    let outcome = spawn::run_child(&task, &binary, store, pc.cancel, pc.observer)?;
    Ok(finish_process(parent.db, store, run_id, &ty.name, outcome, child_depth, pc.now, pc.observer))
}

/// Unregister a run when its monitor thread ends, even on panic.
struct UnregisterOnDrop {
    registry: Arc<ChildRegistry>,
    run_id: String,
}

impl Drop for UnregisterOnDrop {
    fn drop(&mut self) {
        self.registry.unregister(&self.run_id);
    }
}

/// Start a child process and return at once: a monitor thread waits for
/// it, replays its approvals/grants, and stores the handoff for the
/// next turn. Returns `Err` only when the child could not be spawned
/// at all (the caller falls back to in-process).
#[allow(clippy::too_many_arguments)]
pub(crate) fn start_process(
    parent: &Agent,
    d: &Delegation,
    spec: &TaskSpec,
    pc: &ParentCtx,
    store: &RunStore,
    run_id: &str,
    ty: &AgentType,
    top: &str,
    parent_ref: &str,
    registry: &Arc<ChildRegistry>,
) -> Result<StartedRun, SpawnFailure> {
    let child_depth = d.depth + 1;
    let (task, binary) = build_task(parent, d, spec, pc, run_id, ty, top, parent_ref)
        .map_err(|m| SpawnFailure::permanent(m, "配置错误"))?;
    let live = spawn_child(&task, &binary)?;
    let _ = store.set_child_pid(run_id, live.pid());
    let _ = store.start(run_id, &format!("subagent:{run_id}"));
    let cancel = CancelToken::new();
    // Register before the monitor thread starts, so cancel and
    // reconcile see the run from the very beginning.
    if !registry.register(
        run_id,
        RegistryEntry { cancel: cancel.clone(), session: top.to_string(), parent_ref: parent_ref.to_string() },
    ) {
        drop(live); // ChildGuard kills the child and removes the scratch dir.
        return Err(SpawnFailure::permanent(
            format!("子代理 {run_id} 已在运行"),
            "启动失败",
        ));
    }
    let db = registry.db().clone();
    let ty_name = ty.name.clone();
    let now = *pc.now;
    let run_id_owned = run_id.to_string();
    let registry = Arc::clone(registry);
    // The monitor thread never calls the model and never writes
    // user-visible content: the result returns through the handoff
    // table for the next turn.
    std::thread::spawn(move || {
        let _guard = UnregisterOnDrop { registry, run_id: run_id_owned.clone() };
        let store = RunStore::new(&db);
        let outcome = live.wait(&store, &cancel, None);
        let out = finish_process(&db, &store, &run_id_owned, &ty_name, outcome, child_depth, &now, None);
        let _ = store.save_handoff(&run_id_owned, &out.handoff, out.tainted, &now);
    });
    Ok(StartedRun { run_id: run_id.to_string(), agent_type: ty.name.clone(), task: spec.task.clone() })
}

/// Turn a child-process outcome into the run record and handoff.
/// Takes the database (not the parent agent) so the monitor thread can
/// call it after the turn that started the run has ended.
#[allow(clippy::too_many_arguments)]
fn finish_process(
    db: &Db,
    store: &RunStore,
    run_id: &str,
    ty_name: &str,
    outcome: ProcessOutcome,
    child_depth: usize,
    now: &Timestamp,
    observer: Option<&Observer>,
) -> ChildOutcome {
    let approvals = replay_approvals(db, &outcome.approvals, now);
    replay_grants(db, &outcome.consumed_grants, now);
    let _ = store.finish(run_id, outcome.status, &RunResult {
        steps: outcome.steps,
        answer: outcome.answer.clone().unwrap_or_default(),
        error: outcome.error.clone(),
        model_calls: outcome.model_calls,
        prompt_tokens: outcome.prompt_tokens,
        completion_tokens: outcome.completion_tokens,
    });
    if let Some(o) = observer {
        let detail = if outcome.error.is_empty() {
            outcome.status.as_str().to_string()
        } else {
            format!("{}：{}", outcome.status.as_str(), clip(&outcome.error, 60))
        };
        o(&Progress { depth: child_depth, agent: run_id.to_string(), kind: ProgressKind::SubagentDone, detail });
    }
    let mut rep = TurnReport {
        answer: outcome.answer.clone(),
        stop: match outcome.status {
            RunStatus::Completed => StopReason::Answered,
            RunStatus::Cancelled => StopReason::Cancelled(outcome.error.clone()),
            _ => StopReason::ModelError(outcome.error.clone()),
        },
        steps: outcome.steps,
        model_calls: outcome.model_calls,
        usage: Usage { prompt_tokens: outcome.prompt_tokens, completion_tokens: outcome.completion_tokens, ..Default::default() },
        tainted: outcome.tainted,
        approvals: approvals.clone(),
        ..Default::default()
    };
    if !outcome.unavailable_tools.is_empty() {
        let note = format!("（子代理进程未能提供以下工具：{}）", outcome.unavailable_tools.join("、"));
        rep.answer = Some(match rep.answer {
            Some(a) => format!("{a}\n{note}"),
            None => note,
        });
    }
    ChildOutcome {
        handoff: handoff(store, run_id, ty_name, outcome.status, &outcome.error, Some(&rep)),
        tainted: outcome.tainted,
        approvals,
        run_id: run_id.to_string(),
        status: outcome.status,
    }
}
