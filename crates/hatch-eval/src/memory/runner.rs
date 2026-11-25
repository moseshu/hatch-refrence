//! Drive the chat agent through the simulated days and score the result.

use super::oracle::memory_oracle;
use super::report::{
    ApprovalRecord, ClaimRow, DayOutcome, MemoryReport, NightOutcome, SessionOutcome, TargetOutcome,
    TurnOutcome,
};
use super::trace::{Diagnostics, SessionTrace, TraceEvent, TraceTurn};
use super::MemoryWorld;
use crate::report::Usage;
use hatch_agent::{Agent, AgentConfig, RecallMode, RunOptions, SessionStore, StepEvent};
use hatch_core::time::{parse_rfc3339, Timestamp};
use hatch_core::{CancelToken, Error, Result};
use hatch_llm::{ChatRequest, Role, Router, Workload};
use hatch_memory::{MemoryStore, NewClaim};
use hatch_policy::{Approval, ApprovalQueue, ApprovalStatus, AttentionPolicy, Gatekeeper, PolicyConfig};
use hatch_store::Db;
use hatch_tools::builtin::{add_general_tools, default_registry, GeneralTools};
use hatch_tools::ws_exec::WritePayload;
use hatch_tools::{execute_approved, ExecutorRegistry, Workspace, WriteFileExecutor};
use hatch_workers::{MemoryExtractWorker, RouterFactory};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

pub enum MemMode<'a> {
    /// Scripted oracle model; the nightly extraction is skipped (no model).
    Pipeline,
    /// A real model for the chat turns; `extract` builds the router the
    /// nightly extraction worker uses (as the daemon does). `None` skips it.
    Live {
        router: &'a Router,
        extract: Option<RouterFactory>,
    },
}

/// Follow-ups the harness sends per user turn after deciding approvals.
const MAX_ROUNDS: usize = 3;

/// Simulated minutes between turns within a day, so session order is stable.
const TURN_MINUTES: i64 = 2;

/// Tools whose results can show a memory to the model.
const MEMORY_TOOLS: [&str; 2] = ["memory_search", "memory_explain"];

/// Hours after a day's start when the nightly extraction runs (09:30 → 03:30).
const NIGHT_OFFSET_HOURS: i64 = 18;

/// Approval kinds the harness leaves pending: outbound drafts are the
/// thing under test, so they are inspected, never executed or rejected.
const DRAFT_KINDS: [&str; 3] = ["send_email", "create_event", "create_google_event"];

/// The tool surface for this world: memory, calendar, mail drafts and the
/// workspace file tools. No shell, no web, no scheduler — the world tests
/// memory, and every other side-effect path stays closed.
const TOOLS: [&str; 15] = [
    "memory_search",
    "memory_add",
    "memory_explain",
    "calendar_list",
    "calendar_free_slots",
    "propose_email",
    "propose_event",
    "ask_user",
    "fs_read",
    "fs_list",
    "fs_glob",
    "fs_grep",
    "fs_write",
    "fs_edit",
    "approval_status",
];

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

fn ts(s: &str) -> Result<Timestamp> {
    parse_rfc3339(s).map_err(|e| Error::Invalid(format!("{s}: {e}")))
}

/// (rows, prompt tokens, completion tokens) in `usage_log` so far.
fn usage_log_totals(db: &Db) -> Result<(u64, u64, u64)> {
    db.conn()
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(prompt_tokens), 0), COALESCE(SUM(completion_tokens), 0) FROM usage_log",
            [],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)),
        )
        .map(|(n, p, c)| (n.max(0) as u64, p.max(0) as u64, c.max(0) as u64))
        .map_err(|e| Error::Storage(e.to_string()))
}

struct Harness<'a> {
    db: &'a Db,
    world: &'a MemoryWorld,
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
                && r
                    .path_prefix
                    .as_deref()
                    .is_none_or(|pre| p.path.starts_with(pre) && !p.path.contains(".."))
        })
    }

    /// Decide the session's pending non-draft approvals; the message for
    /// the agent, or `None` when nothing was decided.
    fn decide(&self, origin: &str, now: &Timestamp) -> Result<Option<String>> {
        let q = ApprovalQueue::new(self.db);
        let pending: Vec<Approval> = q
            .list(Some(ApprovalStatus::Pending))?
            .into_iter()
            .filter(|a| a.origin == origin && !DRAFT_KINDS.contains(&a.kind.as_str()))
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
            if approve {
                execute_approved(self.db, self.exec, &a.id, now)?;
            }
            lines.push(format!(
                "- {}（{}）：{}",
                a.id,
                a.title,
                if approve { "已批准并执行" } else { "已拒绝" }
            ));
        }
        Ok(Some(format!(
            "审批结果：\n{}\n需要执行结果时可以用 approval_status 查询。请继续。",
            lines.join("\n")
        )))
    }
}

fn session_approvals(db: &Db, origin: &str) -> Result<Vec<ApprovalRecord>> {
    Ok(ApprovalQueue::new(db)
        .list(None)?
        .into_iter()
        .filter(|a| a.origin == origin)
        .map(|a| ApprovalRecord {
            kind: a.kind,
            title: a.title,
            status: a.status.as_str().into(),
            payload: a.payload,
        })
        .collect())
}

fn claim_rows(db: &Db, now: &Timestamp) -> Result<Vec<ClaimRow>> {
    Ok(MemoryStore::new(db)
        .current(now)?
        .into_iter()
        .map(|c| ClaimRow {
            trusted: !c.trust.is_untrusted(),
            trust: format!("{:?}", c.trust),
            id: c.id,
            subject: c.subject,
            text: c.text,
            source: c.source,
            confidence: c.confidence,
            inferred: c.inferred,
            created_at: hatch_core::time::to_rfc3339(&c.created_at),
        })
        .collect())
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

/// System text of a main-workload request: where the fixed profile and the
/// automatically recalled memory are rendered.
fn system_text(req: &ChatRequest) -> String {
    req.messages
        .iter()
        .filter(|m| m.role == Role::System)
        .map(|m| m.text_content())
        .collect::<Vec<_>>()
        .join("\n")
}

/// How a claim appears in a system text: recalled lines carry the
/// provenance suffix `（来源 …）`, fixed-profile lines do not.
fn context_route(system: &str, id: &str) -> Option<&'static str> {
    let tag = format!("[{id}]");
    let line = system.lines().find(|l| l.contains(&tag))?;
    Some(if line.contains("（来源 ") { "自动召回" } else { "固定注入" })
}

/// `recall` selects the automatic recall strategy (production uses
/// [`RecallMode::Auto`]); the other modes exist for controlled comparisons.
pub fn run_memory_eval(world: &MemoryWorld, mode: MemMode, recall: RecallMode) -> Result<MemoryReport> {
    let tmp = tempfile::tempdir()?;
    let base = tmp.path().canonicalize()?;
    let project = base.join("project");
    copy_dir(&world.project_dir(), &project)?;

    let db = Db::open_in_memory()?;
    let mem = MemoryStore::new(&db);
    let mut seeded: BTreeMap<String, String> = BTreeMap::new();
    for c in &world.spec.seed_claims {
        let tags: Vec<&str> = c.tags.iter().map(String::as_str).collect();
        let mut claim = NewClaim::stated(&c.subject, &c.text, "user:chat").with_tags(&tags);
        if let Some(s) = c.salience {
            claim = claim.with_salience(s);
        }
        let id = mem.add(&claim, &ts(&c.at)?)?.id().to_string();
        if let Some(k) = &c.key {
            seeded.insert(k.clone(), id);
        }
    }

    let exec = ExecutorRegistry::new().with(Arc::new(WriteFileExecutor));
    let tools = add_general_tools(
        default_registry(),
        &GeneralTools {
            workspace: Some(Workspace::new(&project)?),
            web_fetch: false,
            search_backend: None,
            max_command_timeout_secs: 60,
        },
    )
    .filtered(&TOOLS);
    let gate = Gatekeeper::new(PolicyConfig::default());
    let oracle_router;
    let (router, extract, mode_name) = match mode {
        MemMode::Pipeline => {
            oracle_router = Router::single(memory_oracle());
            (&oracle_router, None, "pipeline")
        }
        MemMode::Live { router, extract } => (router, extract, "live"),
    };
    // Every main-workload request's system text, drained per session.
    let requests: Arc<Mutex<Vec<String>>> = Arc::default();
    {
        let requests = requests.clone();
        router.observe_requests(Arc::new(move |w: Workload, req: &ChatRequest| {
            if w == Workload::Main {
                lock(&requests).push(system_text(req));
            }
        }));
    }
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
            max_steps: 12,
            recall,
            ..Default::default()
        },
    };
    let harness = Harness {
        db: &db,
        world,
        exec: &exec,
    };
    let sessions = SessionStore::new(&db);
    let worker = MemoryExtractWorker {
        router: extract,
        attention: AttentionPolicy { tz: world.tz, ..Default::default() },
    };

    let mut days = Vec::new();
    let mut nights = Vec::new();
    let mut diagnostics = Diagnostics::default();
    let mut extract_usage = Usage::default();
    let mut clock = ts(&world.spec.days[0].at)?;
    let n_days = world.spec.days.len();
    for (i, day) in world.spec.days.iter().enumerate() {
        let start = ts(&day.at)?;
        clock = clock.max(start);
        let mut outcomes = Vec::new();
        for s in &day.sessions {
            let sid = sessions.create(&format!("eval {}", s.id), &clock)?;
            let origin = format!("chat:{sid}");
            let mut out = SessionOutcome {
                id: s.id.clone(),
                ..Default::default()
            };
            lock(&requests).clear();
            let mut memory_results: Vec<String> = Vec::new();
            let mut session_contexts: Vec<String> = Vec::new();
            let mut trace = SessionTrace {
                day: day.label.clone(),
                session: s.id.clone(),
                turns: Vec::new(),
            };
            for prompt in &s.turns {
                let mut input = prompt.clone();
                for _round in 0..=MAX_ROUNDS {
                    let mut events: Vec<TraceEvent> = Vec::new();
                    let mut hook = |e: StepEvent| {
                        events.extend(TraceEvent::from_step(&e));
                        if let StepEvent::Result { tool, content, .. } = e {
                            if MEMORY_TOOLS.contains(&tool.as_str()) {
                                memory_results.push(content);
                            }
                        }
                    };
                    let r = agent.run(
                        &sid,
                        &input,
                        &clock,
                        RunOptions {
                            on_step_event: Some(&mut hook),
                            ..Default::default()
                        },
                    )?;
                    let turn_contexts = std::mem::take(&mut *lock(&requests));
                    trace.turns.push(TraceTurn {
                        input: hatch_core::secrets::redact(&input),
                        contexts: turn_contexts.iter().map(|c| diagnostics.context(c)).collect(),
                        events,
                        answer: hatch_core::secrets::redact(r.answer.as_deref().unwrap_or("")),
                        stop: format!("{:?}", r.stop),
                        steps: r.steps,
                    });
                    session_contexts.extend(turn_contexts);
                    out.turns.push(TurnOutcome {
                        input: input.clone(),
                        answer: r.answer.clone().unwrap_or_default(),
                        stop: format!("{:?}", r.stop),
                        steps: r.steps,
                        tool_calls: r.tool_calls.clone(),
                    });
                    clock += chrono::Duration::minutes(TURN_MINUTES);
                    match harness.decide(&origin, &clock)? {
                        Some(msg) => input = msg,
                        None => break,
                    }
                }
            }
            out.approvals = session_approvals(&db, &origin)?;
            out.contexts = session_contexts;
            out.memory_results = memory_results;
            diagnostics.sessions.push(trace);
            outcomes.push(out);
        }
        days.push(DayOutcome {
            label: day.label.clone(),
            at: day.at.clone(),
            sessions: outcomes,
        });
        if world.spec.night_extract && i + 1 < n_days {
            let night = start + chrono::Duration::hours(NIGHT_OFFSET_HOURS);
            clock = clock.max(night);
            let before = usage_log_totals(&db)?;
            let mut n = NightOutcome {
                after: day.label.clone(),
                at: hatch_core::time::to_rfc3339(&clock),
                ..Default::default()
            };
            match worker.extract_all(&db, &clock, &CancelToken::new()) {
                Ok(rep) => {
                    n.sessions = rep.sessions;
                    n.added = rep.added.iter().map(|(id, text)| format!("[{id}] {text}")).collect();
                    n.proposals = rep
                        .proposals
                        .iter()
                        .map(|p| format!("{} ≈ {}（相似度 {:.2}）", p.extracted_text, p.existing_text, p.similarity))
                        .collect();
                    n.note = rep.note.clone();
                }
                Err(e) => n.note = Some(format!("提取失败：{e}")),
            }
            let after = usage_log_totals(&db)?;
            extract_usage.calls += after.0.saturating_sub(before.0);
            extract_usage.prompt_tokens += after.1.saturating_sub(before.1);
            extract_usage.completion_tokens += after.2.saturating_sub(before.2);
            nights.push(n);
        }
    }

    let claims = claim_rows(&db, &clock)?;
    let checks = super::report::score(world, &db, &project, &days, &claims)?;
    let targets = target_outcomes(world, &seeded, &days, &checks);
    let totals = router.totals();
    let mut model_errors: Vec<String> = Vec::new();
    for e in router.events() {
        let e: String = hatch_core::secrets::redact(&e).chars().take(300).collect();
        if !model_errors.contains(&e) && model_errors.len() < 5 {
            model_errors.push(e);
        }
    }
    Ok(MemoryReport {
        mode: mode_name.into(),
        model: model_name,
        world: world.spec.name.clone(),
        recall: format!("{recall:?}"),
        days,
        nights,
        claims,
        checks,
        targets,
        diagnostics,
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
        extract_usage,
    })
}

/// What happened to each target memory in its session.
fn target_outcomes(
    world: &MemoryWorld,
    seeded: &BTreeMap<String, String>,
    days: &[DayOutcome],
    checks: &[crate::report::Check],
) -> Vec<TargetOutcome> {
    world
        .spec
        .targets
        .iter()
        .map(|t| {
            let id = seeded.get(&t.key).cloned().unwrap_or_default();
            let session = days.iter().flat_map(|d| d.sessions.iter()).find(|s| s.id == t.session);
            let tag = format!("[{id}]");
            let (first, any, found, searches, questions, steps) = match session {
                None => (None, None, false, 0, 0, 0),
                Some(s) => (
                    s.contexts.first().and_then(|c| context_route(c, &id)),
                    s.contexts.iter().find_map(|c| context_route(c, &id)),
                    s.memory_results.iter().any(|r| r.contains(&tag)),
                    s.turns
                        .iter()
                        .flat_map(|x| x.tool_calls.iter())
                        .filter(|c| c.as_str() == "memory_search")
                        .count(),
                    s.approvals.iter().filter(|a| a.kind == hatch_tools::builtin::ask_user::ASK_KIND).count(),
                    s.turns.iter().map(|x| x.steps).sum::<usize>(),
                ),
            };
            let checks_passed = (!t.checks.is_empty()).then(|| {
                t.checks
                    .iter()
                    .all(|name| checks.iter().any(|c| &c.name == name && c.passed))
            });
            TargetOutcome {
                key: t.key.clone(),
                claim_id: id,
                session: t.session.clone(),
                condition: t.condition.clone(),
                category: t.category,
                checks_passed,
                first_request: first.map(str::to_string),
                any_request: any.map(str::to_string),
                found_by_tool: found,
                memory_searches: searches,
                questions,
                steps,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_passes_every_check() {
        let world = MemoryWorld::load(&super::super::default_world_dir()).unwrap();
        let r = run_memory_eval(&world, MemMode::Pipeline, RecallMode::Auto).unwrap();
        let md = r.render_markdown();
        assert_eq!(r.passed(), r.checks.len(), "{md}");
        assert_eq!(r.checks.len(), world.spec.checks.len(), "{md}");
        // No model for the nightly worker in pipeline mode: skipped, noted.
        assert_eq!(r.nights.len(), 2, "{md}");
        assert!(r.nights.iter().all(|n| n.note.is_some()), "{md}");
        // The seeded preference is still current; the oracle's correction sits beside it.
        assert!(r.claims.iter().any(|c| c.text.contains("张伟") && c.source == "user:chat"), "{md}");
    }

    fn pressure(recall: RecallMode) -> MemoryReport {
        let world = MemoryWorld::load(&super::super::pressure_world_dir()).unwrap();
        let r = run_memory_eval(&world, MemMode::Pipeline, recall).unwrap();
        let md = r.render_markdown();
        assert_eq!(r.checks.len(), world.spec.checks.len(), "{md}");
        assert_eq!(r.passed(), r.checks.len(), "{md}");
        assert_eq!(r.targets.len(), 4, "{md}");
        r
    }

    fn route(r: &MemoryReport, key: &str) -> Option<String> {
        r.targets.iter().find(|t| t.key == key).unwrap().first_request.clone()
    }

    #[test]
    fn pressure_world_records_how_each_target_reached_the_model() {
        let auto = pressure(RecallMode::Auto);
        let md = auto.render_markdown();
        // Beyond the 12-claim profile, reachable only through recall.
        assert_eq!(route(&auto, "signature").as_deref(), Some("自动召回"), "{md}");
        assert_eq!(route(&auto, "contract-owner").as_deref(), Some("自动召回"), "{md}");
        // Weak (unigram-only) or no shared index terms: recall misses them;
        // the oracle finds both with memory_search.
        for key in ["room", "peanut"] {
            let t = auto.targets.iter().find(|t| t.key == key).unwrap();
            assert_eq!(t.first_request, None, "{key}: {md}");
            assert!(t.found_by_tool && t.memory_searches > 0, "{key}: {md}");
        }

        let legacy = pressure(RecallMode::LegacyExcludeAllTagged);
        let md = legacy.render_markdown();
        assert_eq!(route(&legacy, "signature"), None, "tagged claim beyond the profile: {md}");
        assert_eq!(route(&legacy, "contract-owner").as_deref(), Some("自动召回"), "{md}");

        let off = pressure(RecallMode::Off);
        let md = off.render_markdown();
        assert!(off.targets.iter().all(|t| t.first_request.is_none()), "{md}");
        assert!(md.contains("Off"), "{md}");
    }

    #[test]
    fn runs_keep_full_traces_and_the_heldout_summary_reveals_no_task_text() {
        let world = MemoryWorld::load(&super::super::pressure_world_dir()).unwrap();
        let r = pressure(RecallMode::Auto);
        let d = &r.diagnostics;
        assert_eq!(d.sessions.len(), 4);
        for s in &d.sessions {
            assert!(!s.turns.is_empty(), "{}", s.session);
            for t in &s.turns {
                assert!(!t.contexts.is_empty(), "{}: every turn records its contexts", s.session);
                assert!(t.contexts.iter().all(|c| d.contexts.contains_key(c)));
            }
        }
        let lunch = d.sessions.iter().find(|s| s.session == "client-lunch").unwrap();
        let events = serde_json::to_string(&lunch.turns).unwrap();
        assert!(events.contains("memory_search") && events.contains("fs_write"), "{events}");
        let tmp = tempfile::tempdir().unwrap();
        r.write_diagnostics(tmp.path()).unwrap();
        assert_eq!(std::fs::read_dir(tmp.path().join("traces")).unwrap().count(), 4);

        let summary = r.render_heldout_summary(&"ab".repeat(32));
        assert!(summary.contains("检查通过 5/5"), "{summary}");
        assert!(summary.contains("换了说法"), "{summary}");
        let mut secret: Vec<String> = Vec::new();
        for c in &world.spec.checks {
            secret.push(c.name.clone());
        }
        for t in &world.spec.targets {
            secret.extend([t.key.clone(), t.session.clone(), t.condition.clone()]);
        }
        for c in &world.spec.seed_claims {
            secret.push(c.text.clone());
        }
        for d in &world.spec.days {
            for s in &d.sessions {
                secret.extend(s.turns.iter().cloned());
            }
        }
        for t in &r.targets {
            secret.push(t.claim_id.clone());
        }
        for text in secret {
            assert!(!summary.contains(&text), "summary leaks {text:?}:\n{summary}");
        }
    }
}
