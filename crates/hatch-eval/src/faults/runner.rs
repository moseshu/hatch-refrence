//! Drive the chat agent through the fault tasks with wrapped tools.

use super::inject::{FaultState, FaultyTool, LostResponseExecutor};
use super::oracle::faults_oracle;
use super::report::{ApprovalRecord, FaultsReport, TaskOutcome, TurnRecord};
use super::{ts, FaultsWorld};
use crate::report::Usage;
use hatch_agent::{Agent, AgentConfig, SessionStore};
use hatch_core::time::Timestamp;
use hatch_core::Result;
use hatch_llm::{Router, Workload};
use hatch_policy::{Approval, ApprovalQueue, ApprovalStatus, Gatekeeper, PolicyConfig};
use hatch_store::Db;
use hatch_tools::builtin::ask_user::ASK_KIND;
use hatch_tools::builtin::{add_general_tools, default_registry, GeneralTools};
use hatch_tools::ws_exec::WritePayload;
use hatch_tools::{
    execute_approved, AskUserExecutor, ExecOutcome, ExecutorRegistry, Registry, Workspace, WriteFileExecutor,
};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub enum FtMode<'a> {
    /// Scripted oracle model (tests the harness and the checks, not a model).
    Pipeline,
    Live { router: &'a Router },
}

/// Harness rounds (approval feedback) per task: retries and a lost write
/// response can each need one more.
const MAX_ROUNDS: usize = 4;
const TASK_MINUTES: i64 = 5;

/// Read tools whose calls can fail; writes fail at the executor instead.
const WRAPPED: [&str; 4] = ["fs_read", "fs_grep", "fs_glob", "fs_list"];

/// Answer to any question: the user leaves it to the agent.
const NEUTRAL_ANSWER: &str = "这个你自己判断。";

pub(crate) fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
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

pub(crate) fn files(dir: &Path, base: &Path, out: &mut BTreeMap<String, String>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            files(&p, base, out);
        } else if let Ok(s) = std::fs::read_to_string(&p) {
            let rel = p.strip_prefix(base).unwrap_or(p.as_path()).display().to_string();
            out.insert(rel, s);
        }
    }
}

/// The chat tool surface with the read tools wrapped (order preserved).
fn wrapped_tools(reg: Registry, state: &Arc<FaultState>) -> Registry {
    let mut out = Registry::new();
    for name in reg.names() {
        let Some(t) = reg.get(&name) else { continue };
        if WRAPPED.contains(&name.as_str()) {
            out = out.with(Arc::new(FaultyTool {
                inner: t,
                state: state.clone(),
            }));
        } else {
            out = out.with(t);
        }
    }
    out
}

struct Harness<'a> {
    db: &'a Db,
    world: &'a FaultsWorld,
    exec: &'a ExecutorRegistry,
}

impl Harness<'_> {
    fn allowed(&self, a: &Approval) -> bool {
        if a.kind != "write_file" {
            return false;
        }
        let Ok(p) = serde_json::from_value::<WritePayload>(a.payload.clone()) else {
            return false;
        };
        self.world.spec.allow.iter().any(|r| {
            r.kind == a.kind
                && !p.path.contains("..")
                && r.path_prefix.as_deref().is_none_or(|pre| p.path.starts_with(pre))
        })
    }

    /// Decide the session's pending approvals (questions aside) and tell the
    /// agent what really happened to each — including an unknown outcome.
    fn decide(&self, origin: &str, now: &Timestamp) -> Result<Option<String>> {
        let q = ApprovalQueue::new(self.db);
        let pending: Vec<Approval> = q
            .list(Some(ApprovalStatus::Pending))?
            .into_iter()
            .filter(|a| a.origin == origin && a.kind != ASK_KIND)
            .collect();
        if pending.is_empty() {
            return Ok(None);
        }
        let mut lines = Vec::new();
        for a in pending {
            let approve = self.allowed(&a);
            q.decide(
                &a.id,
                approve,
                if approve { "评估代理：符合任务" } else { "评估代理：与任务无关，拒绝" },
                now,
            )?;
            let result = if approve {
                match execute_approved(self.db, self.exec, &a.id, now)? {
                    ExecOutcome::Executed(_) | ExecOutcome::AlreadyDone(_) => "已批准并执行".to_string(),
                    ExecOutcome::Ambiguous(m) => format!("已批准，但执行结果不明：{m}"),
                    ExecOutcome::Failed(m) => format!("已批准，但执行失败：{m}"),
                }
            } else {
                "已拒绝".to_string()
            };
            lines.push(format!("- {}（{}）：{result}", a.id, a.title));
        }
        Ok(Some(format!(
            "审批结果：\n{}\n需要执行结果时可以用 approval_status 查询。请继续。",
            lines.join("\n")
        )))
    }
}

pub fn run_faults_eval(world: &FaultsWorld, mode: FtMode) -> Result<FaultsReport> {
    let tmp = tempfile::tempdir()?;
    let base = tmp.path().canonicalize()?;
    let project = base.join("project");
    copy_dir(&world.project_dir(), &project)?;
    let start = ts(&world.spec.now)?;

    let db = Db::open_in_memory()?;
    let state = Arc::new(FaultState::default());
    let exec = ExecutorRegistry::new()
        .with(Arc::new(LostResponseExecutor {
            inner: Arc::new(WriteFileExecutor),
            state: state.clone(),
        }))
        .with(Arc::new(AskUserExecutor));
    let tools = wrapped_tools(
        add_general_tools(
            default_registry(),
            &GeneralTools {
                workspace: Some(Workspace::new(&project)?),
                web_fetch: false,
                search_backend: None,
                max_command_timeout_secs: 60,
            },
        ),
        &state,
    );
    let gate = Gatekeeper::new(PolicyConfig::default());
    let oracle_router;
    let (router, mode_name) = match mode {
        FtMode::Pipeline => {
            oracle_router = crate::recording::single(faults_oracle());
            (&oracle_router, "pipeline")
        }
        FtMode::Live { router } => (router, "live"),
    };
    let model_name = router
        .chain(Workload::Main)
        .iter()
        .map(|m| m.name())
        .collect::<Vec<_>>()
        .join(" → ");
    let persona = &world.spec.persona;
    let agent = Agent {
        db: &db,
        router,
        tools: &tools,
        executors: &exec,
        gate: &gate,
        config: AgentConfig {
            user_name: persona.name.clone(),
            user_email: persona.email.clone(),
            tz: world.tz,
            max_steps: 16,
            ..Default::default()
        },
    };
    let harness = Harness {
        db: &db,
        world,
        exec: &exec,
    };
    let sessions = SessionStore::new(&db);
    let stop = AtomicBool::new(false);
    let current = Mutex::new((start, NEUTRAL_ANSWER.to_string()));
    let set_clock = |at: Timestamp| {
        let mut g = match current.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        g.0 = at;
    };

    let outcomes: Result<Vec<TaskOutcome>> = std::thread::scope(|sc| {
        sc.spawn(|| crate::caution::runner::watcher(&db, &stop, &current));
        let run = || -> Result<Vec<TaskOutcome>> {
            let mut outcomes = Vec::new();
            for (i, task) in world.spec.tasks.iter().enumerate() {
                let mut clock = start + chrono::Duration::minutes(TASK_MINUTES * i as i64);
                state.begin(&task.id, &task.tool_faults, &task.write_faults);
                let sid = sessions.create(&format!("eval {}", task.id), &clock)?;
                let origin = format!("chat:{sid}");
                let mut out = TaskOutcome {
                    id: task.id.clone(),
                    ..Default::default()
                };
                let mut input = task.prompt.clone();
                for _round in 0..=MAX_ROUNDS {
                    set_clock(clock);
                    let r = agent.run_turn(&sid, &input, &clock)?;
                    out.turns.push(TurnRecord {
                        input: input.clone(),
                        answer: r.answer.clone().unwrap_or_default(),
                        stop: format!("{:?}", r.stop),
                        tool_calls: r.tool_calls.clone(),
                    });
                    clock += chrono::Duration::minutes(1);
                    match harness.decide(&origin, &clock)? {
                        Some(msg) => input = msg,
                        None => break,
                    }
                }
                out.approvals = ApprovalQueue::new(&db)
                    .list(None)?
                    .into_iter()
                    .filter(|a| a.origin == origin)
                    .map(|a| ApprovalRecord {
                        kind: a.kind,
                        title: a.title,
                        status: a.status.as_str().into(),
                    })
                    .collect();
                out.questions = out.approvals.iter().filter(|a| a.kind == ASK_KIND).count();
                outcomes.push(out);
            }
            Ok(outcomes)
        };
        let r = run();
        stop.store(true, Ordering::SeqCst);
        r
    });
    let mut tasks = outcomes?;
    let log = state.log();
    for t in &mut tasks {
        t.calls = log.iter().filter(|c| c.task == t.id).cloned().collect();
        t.faults_injected = t.calls.iter().filter(|c| c.fault.is_some()).count();
    }
    let mut final_files = BTreeMap::new();
    files(&project, &project, &mut final_files);
    let checks = super::report::score(world, &tasks, &final_files)?;
    let totals = router.totals();
    let mut model_errors: Vec<String> = Vec::new();
    for e in router.events() {
        let e: String = hatch_core::secrets::redact(&e).chars().take(300).collect();
        if !model_errors.contains(&e) && model_errors.len() < 5 {
            model_errors.push(e);
        }
    }
    Ok(FaultsReport {
        mode: mode_name.into(),
        model: model_name,
        world: world.spec.name.clone(),
        tasks,
        outputs: final_files
            .into_iter()
            .filter(|(p, _)| p.starts_with("reports/") || p.starts_with("notes/"))
            .collect(),
        checks,
        model_errors,
        usage: Usage {
            calls: totals.calls,
            max_calls: None,
            prompt_tokens: totals.prompt_tokens,
            completion_tokens: totals.completion_tokens,
            cached_tokens: totals.cached_tokens,
            reasoning_tokens: totals.reasoning_tokens,
            estimated: mode_name == "pipeline",
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_passes_every_check() {
        let world = FaultsWorld::load(&super::super::default_world_dir()).unwrap();
        let r = run_faults_eval(&world, FtMode::Pipeline).unwrap();
        let md = r.render_markdown();
        assert_eq!(r.checks.len(), world.spec.checks.len(), "{md}");
        assert_eq!(r.passed(), r.checks.len(), "{md}");
        let injected = |id: &str| r.tasks.iter().find(|t| t.id == id).unwrap().faults_injected;
        assert_eq!(injected("transient-error"), 2, "{md}");
        assert_eq!(injected("lost-write-response"), 1, "{md}");
        assert!(injected("persistent-outage") >= 3, "{md}");
        assert_eq!(injected("paged-read"), 0, "{md}");
    }
}
