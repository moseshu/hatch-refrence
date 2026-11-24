//! Drive the chat agent through the simulated days and score the result.

use super::oracle::memory_oracle;
use super::report::{
    ApprovalRecord, ClaimRow, DayOutcome, MemoryReport, NightOutcome, SessionOutcome, TurnOutcome,
};
use super::MemoryWorld;
use crate::report::Usage;
use hatch_agent::{Agent, AgentConfig, SessionStore};
use hatch_core::time::{parse_rfc3339, Timestamp};
use hatch_core::{CancelToken, Error, Result};
use hatch_llm::{Router, Workload};
use hatch_memory::{MemoryStore, NewClaim};
use hatch_policy::{Approval, ApprovalQueue, ApprovalStatus, AttentionPolicy, Gatekeeper, PolicyConfig};
use hatch_store::Db;
use hatch_tools::builtin::{add_general_tools, default_registry, GeneralTools};
use hatch_tools::ws_exec::WritePayload;
use hatch_tools::{execute_approved, ExecutorRegistry, Workspace, WriteFileExecutor};
use hatch_workers::{MemoryExtractWorker, RouterFactory};
use std::path::Path;
use std::sync::Arc;

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

pub fn run_memory_eval(world: &MemoryWorld, mode: MemMode) -> Result<MemoryReport> {
    let tmp = tempfile::tempdir()?;
    let base = tmp.path().canonicalize()?;
    let project = base.join("project");
    copy_dir(&world.project_dir(), &project)?;

    let db = Db::open_in_memory()?;
    let mem = MemoryStore::new(&db);
    for c in &world.spec.seed_claims {
        mem.add(&NewClaim::stated(&c.subject, &c.text, "user:chat"), &ts(&c.at)?)?;
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
        attention: AttentionPolicy::default(),
    };

    let mut days = Vec::new();
    let mut nights = Vec::new();
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
            for prompt in &s.turns {
                let mut input = prompt.clone();
                for _round in 0..=MAX_ROUNDS {
                    let r = agent.run_turn(&sid, &input, &clock)?;
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
        days,
        nights,
        claims,
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
        extract_usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_passes_every_check() {
        let world = MemoryWorld::load(&super::super::default_world_dir()).unwrap();
        let r = run_memory_eval(&world, MemMode::Pipeline).unwrap();
        let md = r.render_markdown();
        assert_eq!(r.passed(), r.checks.len(), "{md}");
        assert_eq!(r.checks.len(), world.spec.checks.len(), "{md}");
        // No model for the nightly worker in pipeline mode: skipped, noted.
        assert_eq!(r.nights.len(), 2, "{md}");
        assert!(r.nights.iter().all(|n| n.note.is_some()), "{md}");
        // The seeded preference is still current; the oracle's correction sits beside it.
        assert!(r.claims.iter().any(|c| c.text.contains("张伟") && c.source == "user:chat"), "{md}");
    }
}
