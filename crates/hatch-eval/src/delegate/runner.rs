//! Drive the chat agent (with delegation) through the four tasks.

use super::oracle::delegate_oracle;
use super::report::{score, ApprovalRecord, DelegateReport, RunBrief, TaskResult};
use super::DelegateWorld;
use crate::report::Usage;
use crate::workspace::report::ApprovalOutcome;
use hatch_agent::{
    Agent, AgentConfig, Delegation, Progress, ProgressKind, RunOptions, RunStore, SessionStore,
    TypeRegistry,
};
use hatch_core::{CancelToken, Result};
use hatch_llm::{Router, Workload};
use hatch_policy::{
    ActionClass, ApprovalQueue, ApprovalStatus, Autonomy, Gatekeeper, GrantStore, NewGrant,
    PolicyConfig,
};
use hatch_sandbox::{Sandbox, SandboxMode};
use hatch_store::Db;
use hatch_tools::builtin::{add_general_tools, GeneralTools};
use hatch_tools::{
    ExecCommandExecutor, ExecutorRegistry, FetchUrlExecutor, Registry, Workspace, WriteFileExecutor,
};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub enum DgMode<'a> {
    /// Scripted oracle model (tests the harness and the defences, not a model).
    Pipeline,
    Live {
        router: &'a Router,
    },
}

pub(crate) const CANCEL_REASON: &str = "Ctrl-C（评估代理模拟）";

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let to = dst.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_dir(&e.path(), &to)?;
        } else {
            std::fs::copy(e.path(), &to)?;
        }
    }
    Ok(())
}

/// Reject every pending approval (this world has nothing that needs one).
fn reject_pending(db: &Db, out: &mut Vec<ApprovalOutcome>) -> Result<()> {
    let q = ApprovalQueue::new(db);
    let now = hatch_core::time::now();
    for a in q.list(Some(ApprovalStatus::Pending))? {
        q.decide(&a.id, false, "评估代理：与任务无关，拒绝", &now)?;
        out.push(ApprovalOutcome {
            kind: a.kind,
            title: a.title,
            reasons: a.reasons,
            approved: false,
            status: "rejected".into(),
        });
    }
    Ok(())
}

/// Every approval decided or executed for `session`'s tree, by origin.
fn approvals_of(db: &Db, origins: &[String]) -> Result<Vec<ApprovalRecord>> {
    Ok(ApprovalQueue::new(db)
        .list(None)?
        .into_iter()
        .filter(|a| origins.contains(&a.origin))
        .map(|a| ApprovalRecord {
            status: a.status.as_str().to_string(),
            payload: a.payload.to_string(),
            origin: a.origin,
            kind: a.kind,
            reasons: a.reasons,
        })
        .collect())
}

struct Ctx<'a> {
    agent: Agent<'a>,
    db: &'a Db,
}

impl Ctx<'_> {
    fn task(
        &self,
        id: &str,
        prompt: &str,
        cancel_rule: Option<(usize, u64)>,
    ) -> Result<(TaskResult, String)> {
        let now = hatch_core::time::now();
        let sid = SessionStore::new(self.db).create(&format!("eval {id}"), &now)?;
        let token = CancelToken::new();
        let done = CancelToken::new();
        let sub_tool_calls = AtomicUsize::new(0);
        let events = Mutex::new(0usize);
        let observe = |p: &Progress| {
            *events.lock().unwrap() += 1;
            if let Some((after, _)) = cancel_rule {
                if p.depth > 0
                    && p.kind == ProgressKind::ToolCall
                    && sub_tool_calls.fetch_add(1, Ordering::SeqCst) + 1 == after
                {
                    let t = token.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_millis(500));
                        t.cancel(CANCEL_REASON);
                    });
                }
            }
        };
        if let Some((_, secs)) = cancel_rule {
            let (t, d) = (token.clone(), done.clone());
            std::thread::spawn(move || {
                if !d.sleep(Duration::from_secs(secs)) {
                    t.cancel(CANCEL_REASON);
                }
            });
        }
        let started = Instant::now();
        let r = self.agent.run(
            &sid,
            prompt,
            &now,
            RunOptions {
                on_delta: None,
                cancel: token.clone(),
                observer: Some(&observe),
            },
        );
        done.cancel("turn finished");
        let r = r?;
        let mut result = TaskResult {
            id: id.into(),
            stop: format!("{:?}", r.stop),
            cancelled: r.is_cancelled(),
            answer: r.answer.clone().unwrap_or_default(),
            receipt: if r.answer.is_some() {
                String::new()
            } else {
                r.receipt()
            },
            steps: r.steps,
            tool_calls: r.tool_calls.clone(),
            delegated: r.tool_calls.iter().any(|t| t.starts_with("delegate")),
            elapsed_ms: started.elapsed().as_millis() as u64,
            progress_events: *events.lock().unwrap(),
            ..Default::default()
        };
        reject_pending(self.db, &mut result.approvals)?;
        Ok((result, sid))
    }

    fn runs(&self, sid: &str) -> Result<Vec<RunBrief>> {
        Ok(RunStore::new(self.db)
            .list(Some(sid), 100)?
            .into_iter()
            .rev()
            .map(|r| RunBrief {
                id: r.id,
                depth: r.depth,
                agent_type: r.agent_type,
                status: r.status.as_str().into(),
                error: r.error,
            })
            .collect())
    }
}

/// Wait until no `sleep <marker>` process is left (up to `wait`).
pub(crate) fn processes_gone(marker: &str, wait: Duration) -> bool {
    let alive = || {
        std::fs::read_dir("/proc").map(|d| {
            d.flatten().any(|e| {
                let cmd = std::fs::read(e.path().join("cmdline")).unwrap_or_default();
                let parts: Vec<&[u8]> = cmd.split(|b| *b == 0).collect();
                parts.len() >= 2 && parts[0].ends_with(b"sleep") && parts[1] == marker.as_bytes()
            })
        })
    };
    let deadline = Instant::now() + wait;
    loop {
        match alive() {
            Ok(false) => return true,
            Err(_) => return true, // no /proc: nothing to inspect
            Ok(true) if Instant::now() >= deadline => return false,
            Ok(true) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

pub fn run_delegate_eval(
    world: &DelegateWorld,
    mode: DgMode,
    sandbox: Sandbox,
) -> Result<DelegateReport> {
    let spec = &world.spec;
    let tmp = tempfile::tempdir()?;
    let project = tmp.path().canonicalize()?.join("project");
    copy_dir(&world.project_dir(), &project)?;
    let originals: Vec<(String, Vec<u8>)> = spec
        .injection
        .unchanged_files
        .iter()
        .map(|f| {
            (
                f.clone(),
                std::fs::read(project.join(f)).unwrap_or_default(),
            )
        })
        .collect();
    let db = Db::open_in_memory()?;
    let now = hatch_core::time::now();
    for g in &spec.grants {
        let class = ActionClass::parse(&g.class)?;
        let grant = NewGrant {
            class,
            recipients: vec![g.pattern.clone()],
            max_uses: None,
            allow_when_tainted: false,
            expires_at: None,
            note: "评估世界授权".into(),
        };
        GrantStore::new(&db).add(&grant, &now)?;
    }
    let sandbox_desc = match sandbox.mode {
        SandboxMode::None => "未隔离（mode=none）".to_string(),
        SandboxMode::Namespaces => sandbox
            .probe()
            .map(|r| r.summary())
            .unwrap_or_else(|e| format!("不可用：{e}")),
    };
    let exec = ExecutorRegistry::new()
        .with(Arc::new(WriteFileExecutor))
        .with(Arc::new(ExecCommandExecutor {
            sandbox: Arc::new(sandbox),
        }))
        .with(Arc::new(FetchUrlExecutor {
            limits: Default::default(),
        }));
    let ws = Workspace::new(&project)?;
    let tools = add_general_tools(
        Registry::new(),
        &GeneralTools {
            workspace: Some(ws),
            web_fetch: true,
            search_backend: None,
            max_command_timeout_secs: 60,
        },
    );
    let mut policy = PolicyConfig::default();
    policy.set(ActionClass::FetchWeb, Autonomy::Draft);
    let gate = Gatekeeper::new(policy);
    let oracle_router;
    let (router, mode_name) = match mode {
        DgMode::Pipeline => {
            oracle_router = Router::single(delegate_oracle());
            (&oracle_router, "pipeline")
        }
        DgMode::Live { router } => (router, "live"),
    };
    let model_name = router
        .chain(Workload::Main)
        .iter()
        .map(|m| m.name())
        .collect::<Vec<_>>()
        .join(" → ");
    let mut delegation = Delegation::root(Arc::new(TypeRegistry::builtin()));
    delegation.max_depth = spec.max_depth;
    delegation.max_parallel = spec.max_parallel;
    let config = AgentConfig {
        user_name: spec.persona.name.clone(),
        user_email: spec.persona.email.clone(),
        max_steps: 12,
        delegation: Some(delegation),
        ..Default::default()
    };
    let ctx = Ctx {
        agent: Agent {
            db: &db,
            router,
            tools: &tools,
            executors: &exec,
            gate: &gate,
            config,
        },
        db: &db,
    };

    let (mut split, sid) = ctx.task("split", &spec.split.prompt, None)?;
    split.runs = ctx.runs(&sid)?;

    let c = &spec.cancel;
    let (mut cancel, csid) = ctx.task(
        "cancel",
        &c.prompt,
        Some((
            c.cancel_after_subagent_tool_calls.max(1),
            c.cancel_after_secs,
        )),
    )?;
    cancel.runs = ctx.runs(&csid)?;
    cancel.processes_gone = processes_gone(&c.process_marker, Duration::from_secs(3));
    let follow = ctx
        .agent
        .run_turn(&csid, &c.follow_up, &hatch_core::time::now())?;
    cancel.follow_up_stop = format!("{:?}", follow.stop);
    cancel.follow_up_answer = follow.answer.unwrap_or_default();

    let (mut depth, dsid) = ctx.task("depth", &spec.depth.prompt, None)?;
    depth.runs = ctx.runs(&dsid)?;

    let (mut injection, isid) = ctx.task("injection", &spec.injection.prompt, None)?;
    injection.runs = ctx.runs(&isid)?;
    let origins: Vec<String> = injection
        .runs
        .iter()
        .map(|r| format!("subagent:{}", r.id))
        .chain([format!("chat:{isid}")])
        .collect();
    injection.approval_records = approvals_of(&db, &origins)?;

    let tasks = vec![split, cancel, depth, injection];
    let checks = score(
        world,
        &db,
        &project,
        &originals,
        &tasks,
        mode_name == "pipeline",
    )?;
    let totals = router.totals();
    let mut model_errors: Vec<String> = Vec::new();
    for e in router.events() {
        let e: String = hatch_core::secrets::redact(&e).chars().take(300).collect();
        if !model_errors.contains(&e) && model_errors.len() < 5 {
            model_errors.push(e);
        }
    }
    Ok(DelegateReport {
        mode: mode_name.into(),
        model: model_name,
        world: spec.name.clone(),
        sandbox: sandbox_desc,
        max_depth: spec.max_depth,
        tasks,
        checks,
        model_errors,
        usage: Usage {
            calls: totals.calls,
            max_calls: None,
            prompt_tokens: totals.prompt_tokens,
            completion_tokens: totals.completion_tokens,
            cached_tokens: totals.cached_tokens,
            reasoning_tokens: totals.reasoning_tokens,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatch_sandbox::{HelperCmd, Limits};

    #[test]
    fn pipeline_passes_every_check_with_a_misbehaving_model() {
        let sandbox = match HelperCmd::locate() {
            Some(h) => Sandbox {
                mode: SandboxMode::Namespaces,
                helper: Some(h),
                limits: Limits::default(),
                max_output: 16 * 1024,
            },
            None => Sandbox {
                mode: SandboxMode::None,
                helper: None,
                limits: Limits::default(),
                max_output: 16 * 1024,
            },
        };
        let world = DelegateWorld::load(&super::super::default_world_dir()).unwrap();
        let r = run_delegate_eval(&world, DgMode::Pipeline, sandbox).unwrap();
        let md = r.render_markdown();
        assert_eq!(r.passed(), r.checks.len(), "{md}");
        let cancel = r.tasks.iter().find(|t| t.id == "cancel").unwrap();
        assert!(cancel.elapsed_ms < 20_000, "cancelled promptly: {md}");
        assert_eq!(cancel.runs.len(), 2);
    }
}
