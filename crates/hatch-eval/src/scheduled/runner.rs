//! Store the jobs, play the timeline through the real scheduler engine and
//! `PromptWorker`, then score the resulting state.

use super::oracle::scheduled_oracle;
use super::report::{ApprovalRecord, OutputRecord, RunRecord, ScheduledReport};
use super::{ts, Action, ScheduledWorld};
use crate::report::Usage;
use hatch_core::{CancelToken, Error, Result};
use hatch_llm::Router;
use hatch_policy::{ActionClass, ApprovalQueue, AttentionPolicy, GrantStore, NewGrant, PolicyConfig};
use hatch_scheduler::{Engine, JobStore, NewJob, RunResult};
use hatch_store::Db;
use hatch_tools::builtin::{add_general_tools, default_registry, GeneralTools};
use hatch_tools::{ExecutorRegistry, Workspace, WriteFileExecutor};
use hatch_workers::{prompt_job, PromptWorker, RouterFactory, WorkerOutputs};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

pub enum StMode {
    /// Scripted oracle model (tests the harness and the checks, not a model).
    Pipeline,
    /// A real model; `factory` builds a fresh router per job run, as the
    /// daemon does.
    Live { factory: RouterFactory, label: String },
}

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

/// Every text file under `dir`, keyed by workspace-relative path.
fn files(dir: &Path, base: &Path, out: &mut BTreeMap<String, String>) {
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

/// Inside the workspace only: no absolute paths, no `..`.
fn safe_rel(path: &str) -> Result<&str> {
    if path.starts_with('/') || path.split('/').any(|s| s == "..") {
        return Err(Error::Invalid(format!("timeline path escapes the workspace: {path}")));
    }
    Ok(path)
}

fn record(at: &str, r: &RunResult) -> RunRecord {
    RunRecord {
        at: at.to_string(),
        job: r.job_name.clone(),
        trigger: format!("{:?}", r.trigger),
        status: format!("{:?}", r.status),
        ok: r.status == hatch_scheduler::RunStatus::Ok,
        summary: r.summary.clone(),
        error: r.error.clone(),
    }
}

/// Tool names called in each job's session (`scheduled:<job>`), in order.
fn job_tool_calls(db: &Db) -> Result<BTreeMap<String, Vec<String>>> {
    let conn = db.conn();
    let mut st = conn
        .prepare(
            "SELECT s.title, m.meta FROM session_messages m JOIN sessions s ON s.id = m.session_id
             WHERE s.title LIKE 'scheduled:%' AND m.role = 'assistant' ORDER BY s.title, m.seq",
        )
        .map_err(|e| Error::Storage(e.to_string()))?;
    let rows = st
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(|e| Error::Storage(e.to_string()))?;
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (title, meta) in rows.flatten() {
        let job = title.trim_start_matches("scheduled:").to_string();
        let names: Vec<String> = serde_json::from_str::<Value>(&meta)
            .ok()
            .and_then(|m| m.get("tool_calls").and_then(Value::as_array).cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|c| c.get("name").and_then(Value::as_str).map(str::to_string))
            .collect();
        out.entry(job).or_default().extend(names);
    }
    Ok(out)
}

/// (rows, prompt tokens, completion tokens, cached tokens) in `usage_log`.
fn usage_totals(db: &Db) -> Result<(u64, u64, u64, u64)> {
    db.conn()
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(prompt_tokens), 0), COALESCE(SUM(completion_tokens), 0), COALESCE(SUM(cached_tokens), 0) FROM usage_log",
            [],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            },
        )
        .map(|(n, p, c, k)| (n.max(0) as u64, p.max(0) as u64, c.max(0) as u64, k.max(0) as u64))
        .map_err(|e| Error::Storage(e.to_string()))
}

pub fn run_scheduled_eval(world: &ScheduledWorld, mode: StMode) -> Result<ScheduledReport> {
    let tmp = tempfile::tempdir()?;
    let base = tmp.path().canonicalize()?;
    let project = base.join("project");
    copy_dir(&world.project_dir(), &project)?;
    let spec = &world.spec;
    let created = ts(&spec.created_at)?;

    let db = Db::open_in_memory()?;
    for g in &spec.grants {
        GrantStore::new(&db).add(
            &NewGrant {
                class: ActionClass::parse(&g.class)?,
                recipients: vec![g.pattern.clone()],
                max_uses: None,
                allow_when_tainted: g.allow_when_tainted,
                expires_at: None,
                note: "评估世界：用户事先给的常设授权".into(),
            },
            &created,
        )?;
    }

    // The chat tool surface (as the daemon builds it), minus web, artifacts
    // and skills. Only workspace writes have an executor: anything else a
    // job proposes can only ever wait in the approval queue.
    let tools = add_general_tools(
        default_registry(),
        &GeneralTools {
            workspace: Some(Workspace::new(&project)?),
            web_fetch: false,
            search_backend: None,
            max_command_timeout_secs: 60,
        },
    );
    let (factory, mode_name, model): (RouterFactory, &str, String) = match mode {
        StMode::Pipeline => (
            Arc::new(|| Ok(Router::single(scheduled_oracle()))),
            "pipeline",
            "scheduled-oracle".into(),
        ),
        StMode::Live { factory, label } => (factory, "live", label),
    };
    let worker = PromptWorker {
        router: Some(factory),
        policy: PolicyConfig::default(),
        executors: Arc::new(ExecutorRegistry::new().with(Arc::new(WriteFileExecutor))),
        tools,
        attention: AttentionPolicy {
            tz: world.tz,
            ..Default::default()
        },
        user_name: spec.persona.name.clone(),
        user_email: spec.persona.email.clone(),
        max_steps: 12,
        max_tokens: None,
        skills: Vec::new(),
        skill_tools: None,
    };
    let engine = Engine::new(&db, world.tz).register(prompt_job::KIND, Arc::new(worker));
    let store = JobStore::new(&db);
    for j in &spec.jobs {
        let mut nj = NewJob::new(&j.name, prompt_job::KIND, &j.schedule);
        nj.params = json!({ "prompt": j.prompt });
        nj.scope = j.scope.clone();
        store.add(&nj, &created, &world.tz)?;
    }

    let cancel = CancelToken::new();
    let mut runs = Vec::new();
    let mut missed: Vec<String> = Vec::new();
    let mut snapshots: BTreeMap<String, Option<String>> = BTreeMap::new();
    for step in &spec.timeline {
        let at = ts(&step.at)?;
        match &step.action {
            Action::Tick => {
                let t = engine.tick(&at, &cancel)?;
                runs.extend(t.runs.iter().map(|r| record(&step.at, r)));
                missed.extend(t.missed.iter().map(|(job, n)| format!("{} {job} ×{n}", step.at)));
            }
            Action::Run { job } => {
                let r = engine.run_now(job, &at, &cancel)?;
                runs.push(record(&step.at, &r));
            }
            Action::Write { path, content } => {
                let p = project.join(safe_rel(path)?);
                if let Some(dir) = p.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::write(&p, content)?;
            }
            Action::Snapshot { path } => {
                snapshots.insert(path.clone(), std::fs::read_to_string(project.join(safe_rel(path)?)).ok());
            }
        }
    }

    let approvals: Vec<ApprovalRecord> = ApprovalQueue::new(&db)
        .list(None)?
        .into_iter()
        .map(|a| ApprovalRecord {
            origin: a.origin,
            kind: a.kind,
            title: a.title,
            status: a.status.as_str().into(),
            payload: a.payload,
        })
        .collect();
    let mut outputs: Vec<OutputRecord> = WorkerOutputs::new(&db)
        .recent(100)?
        .into_iter()
        .filter(|o| o.kind == prompt_job::KIND)
        .map(|o| OutputRecord {
            at: hatch_core::time::to_rfc3339(&o.created_at),
            title: o.title,
            content: o.content,
        })
        .collect();
    outputs.reverse();
    let tool_calls = job_tool_calls(&db)?;
    let mut final_files = BTreeMap::new();
    files(&project, &project, &mut final_files);

    let checks = super::report::score(world, &final_files, &snapshots, &runs, &approvals, &tool_calls)?;
    let (calls, prompt_tokens, completion_tokens, cached_tokens) = usage_totals(&db)?;
    Ok(ScheduledReport {
        mode: mode_name.into(),
        model,
        world: spec.name.clone(),
        runs,
        missed,
        approvals,
        outputs,
        tool_calls,
        reports: final_files
            .into_iter()
            .filter(|(p, _)| p.starts_with("reports/"))
            .collect(),
        checks,
        usage: Usage {
            calls,
            max_calls: None,
            prompt_tokens,
            completion_tokens,
            cached_tokens,
            reasoning_tokens: 0,
            estimated: mode_name == "pipeline",
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_passes_every_check() {
        let world = ScheduledWorld::load(&super::super::default_world_dir()).unwrap();
        let r = run_scheduled_eval(&world, StMode::Pipeline).unwrap();
        let md = r.render_markdown();
        assert_eq!(r.checks.len(), world.spec.checks.len(), "{md}");
        assert_eq!(r.passed(), r.checks.len(), "{md}");
        // weekly-todo ×2, weekly-mail ×1, daily-kpi ×3 (two ticks + one manual re-run).
        assert_eq!(r.runs.len(), 6, "{md}");
        // The 11-16 tick finds daily-kpi's 11-11..11-15 firings missed (catch_up = none).
        assert!(!r.missed.is_empty(), "{md}");
    }
}
