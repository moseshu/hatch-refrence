//! Drive the chat agent through the real-web tasks.

use super::net::{FixtureFetch, Fixtures, Recording, Truth, WebLog};
use super::oracle::web_oracle;
use super::report::{ApprovalRecord, TaskOutcome, TurnRecord, WebReport};
use super::{ts, WebWorld};
use crate::faults::runner::{copy_dir, files};
use crate::report::Usage;
use chrono::Datelike;
use hatch_agent::{Agent, AgentConfig, SessionStore};
use hatch_core::time::Timestamp;
use hatch_core::Result;
use hatch_llm::{Router, Workload};
use hatch_policy::{Approval, ApprovalQueue, ApprovalStatus, Gatekeeper, PolicyConfig};
use hatch_store::Db;
use hatch_tools::builtin::ask_user::ASK_KIND;
use hatch_tools::builtin::{add_general_tools, default_registry, GeneralTools};
use hatch_tools::web::{FetchLimits, SearchBackend};
use hatch_tools::ws_exec::WritePayload;
use hatch_tools::{
    execute_approved, AskUserExecutor, ExecOutcome, ExecutorRegistry, FetchUrlExecutor, WebSearchExecutor,
    Workspace, WriteFileExecutor,
};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub enum WlMode<'a> {
    /// Scripted oracle model and fixture pages (tests the harness and the
    /// checks offline, not a model and not the network).
    Pipeline,
    /// Replay a frozen response tape against the same offline fixture pages.
    PipelineReplay(Arc<dyn hatch_llm::ChatModel>),
    /// A real model on the real network. `search` is the configured search
    /// backend, if any; without one the agent only has `web_fetch`.
    Live {
        router: &'a Router,
        search: Option<Arc<dyn SearchBackend>>,
        fetch: FetchLimits,
    },
}

/// Harness rounds (approval feedback) per task.
const MAX_ROUNDS: usize = 3;
const TASK_MINUTES: i64 = 5;

/// Answer to any question: the user leaves it to the agent.
const NEUTRAL_ANSWER: &str = "这个你自己判断。";

struct Harness<'a> {
    db: &'a Db,
    world: &'a WebWorld,
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
    /// agent what really happened to each.
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

pub fn run_web_eval(world: &WebWorld, mode: WlMode) -> Result<WebReport> {
    let tmp = tempfile::tempdir()?;
    let base = tmp.path().canonicalize()?;
    let project = base.join("project");
    copy_dir(&world.project_dir(), &project)?;
    std::fs::create_dir_all(project.join("reports"))?;
    let start = ts(&world.spec.now)?;
    let year = start.with_timezone(&world.tz).year();

    let db = Db::open_in_memory()?;
    let log = Arc::new(WebLog::default());
    let oracle_router;
    let (router, mode_name, fetch, search, truth): (
        &Router,
        &str,
        Arc<dyn hatch_tools::ActionExecutor>,
        Option<Arc<dyn SearchBackend>>,
        Truth,
    ) = match mode {
        offline @ (WlMode::Pipeline | WlMode::PipelineReplay(_)) => {
            let pages = Arc::new(Fixtures::load(&world.fixtures_dir())?);
            let model = match offline {
                WlMode::PipelineReplay(model) => model,
                _ => {
                    let version = pages.get("https://pypi.org/pypi/requests/json")
                        .and_then(|page| serde_json::from_str::<serde_json::Value>(&page.text).ok())
                        .and_then(|body| body.pointer("/info/version").and_then(|v| v.as_str()).map(str::to_owned))
                        .unwrap_or_default();
                    web_oracle(version)
                }
            };
            oracle_router = crate::recording::single(model);
            (
                &oracle_router,
                "pipeline",
                Arc::new(FixtureFetch { pages: pages.clone() }) as Arc<dyn hatch_tools::ActionExecutor>,
                None,
                Truth::Fixture(pages),
            )
        }
        WlMode::Live { router, search, fetch } => (
            router,
            "live",
            Arc::new(FetchUrlExecutor { limits: fetch.clone() }) as Arc<dyn hatch_tools::ActionExecutor>,
            search,
            Truth::Live(fetch),
        ),
    };
    let search_name = search.as_ref().map(|s| s.name());
    let mut exec = ExecutorRegistry::new()
        .with(Arc::new(WriteFileExecutor))
        .with(Arc::new(AskUserExecutor))
        .with(Arc::new(Recording {
            inner: fetch,
            log: log.clone(),
        }));
    if let Some(backend) = search.clone() {
        exec = exec.with(Arc::new(Recording {
            inner: Arc::new(WebSearchExecutor { backend }),
            log: log.clone(),
        }));
    }
    let tools = add_general_tools(
        default_registry(),
        &GeneralTools {
            workspace: Some(Workspace::new(&project)?),
            web_fetch: true,
            search_backend: search_name.clone(),
            max_command_timeout_secs: 60,
        },
    );
    let gate = Gatekeeper::new(PolicyConfig::default());
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
                log.begin(&task.id);
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
    let records = log.records();
    for t in &mut tasks {
        t.web = records.iter().filter(|r| r.task == t.id).cloned().collect();
    }
    let mut final_files = BTreeMap::new();
    files(&project, &project, &mut final_files);
    let checks = super::report::score(world, &tasks, &final_files, &truth, year)?;
    let totals = router.totals();
    let mut model_errors: Vec<String> = Vec::new();
    for e in router.events() {
        let e: String = hatch_core::secrets::redact(&e).chars().take(300).collect();
        if !model_errors.contains(&e) && model_errors.len() < 5 {
            model_errors.push(e);
        }
    }
    Ok(WebReport {
        mode: mode_name.into(),
        model: model_name,
        world: world.spec.name.clone(),
        search: search_name,
        tasks,
        outputs: final_files.into_iter().filter(|(p, _)| p.starts_with("reports/")).collect(),
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
        let world = WebWorld::load(&super::super::default_world_dir()).unwrap();
        let r = run_web_eval(&world, WlMode::Pipeline).unwrap();
        let md = r.render_markdown();
        assert_eq!(r.checks.len(), world.spec.checks.len(), "{md}");
        assert_eq!(r.passed(), r.checks.len(), "{md}");
        let fetches = |id: &str| r.tasks.iter().find(|t| t.id == id).unwrap().web.len();
        assert!(fetches("dead-link") >= 1, "{md}");
        assert!(fetches("unreachable-host") >= 1, "{md}");
        assert!(r.tasks.iter().all(|t| !t.web.is_empty()), "every task touches the web: {md}");
    }
}
