//! The deterministic (non-Agent) step runners of the M0 task worker:
//! `triage` (the triage workflow over the fixture mailbox) and
//! `artifact` (write the brief file, versioned by prior
//! `artifact_written` events).

use hatch_connectors::mail::fixture::{FileOutbox, FixtureMailbox};
use hatch_core::time::Timestamp;
use hatch_core::{Error, Result};
use hatch_eval::oracle::oracle_model;
use hatch_eval::World;
use hatch_llm::Router;
use hatch_policy::{AttentionPolicy, Gatekeeper, PolicyConfig};
use hatch_store::Db;
use hatch_tasks::{EventLog, ExecStatus, Lease, UnitCallSpec, UnitStore};
use hatch_tools::{CreateEventExecutor, ExecutorRegistry, SendEmailExecutor};
use hatch_triage::{TriageConfig, TriageWorkflow};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Step 1: run the triage workflow over the fixture mailbox.
pub(crate) fn run_triage(
    db: &Db,
    world: &World,
    now: &Timestamp,
    tz: &chrono_tz::Tz,
    out_dir: &Path,
) -> Result<hatch_triage::Receipt> {
    let account = world.persona.email.clone();
    let outbox = Arc::new(FileOutbox::new(out_dir, &account)?);
    let exec = ExecutorRegistry::new()
        .with(Arc::new(SendEmailExecutor::new(outbox.clone(), &account)))
        .with(Arc::new(CreateEventExecutor));
    let gate = Gatekeeper::new(PolicyConfig::default());
    let router = Router::single(oracle_model(world));
    let wf = TriageWorkflow {
        db,
        router: &router,
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
    wf.run(
        &mut FixtureMailbox::new(&account, world.mailbox.clone()),
        now,
    )
}

/// Step 3: write the brief artifact, versioned by the count of prior
/// `artifact_written` events for this task (so recovery never writes
/// the same version twice and never skips one).
pub(crate) fn write_artifact(
    db: &Db,
    task_id: &str,
    lease: &Lease,
    units: &UnitStore,
    out_dir: &Path,
    now: &Timestamp,
) -> Result<PathBuf> {
    let log = EventLog::new(db);
    let events = log.events_since(task_id, 0, 1000)?;
    let version = events
        .iter()
        .filter(|e| e.kind == "artifact_written")
        .count()
        + 1;
    let gather = units
        .get_step(task_id, "gather")?
        .ok_or_else(|| Error::NotFound("gather 步骤不存在".into()))?;
    let path = out_dir.join(format!("brief-v{version}.md"));
    std::fs::create_dir_all(out_dir)
        .map_err(|e| Error::Invalid(format!("创建输出目录失败：{e}")))?;
    std::fs::write(&path, &gather.output)
        .map_err(|e| Error::Invalid(format!("写简报失败：{e}")))?;
    let _ = log.append_event(
        task_id,
        lease,
        "artifact_written",
        "artifact",
        &format!("{} (v{version})", path.display()),
        now,
    );
    let unit = units.begin_unit(
        task_id,
        lease,
        "artifact",
        &[UnitCallSpec {
            id: "artifact.write".into(),
            tool: "artifact.write".into(),
            arguments: json!({"path": path.to_string_lossy(), "version": version}),
        }],
        now,
    )?;
    let _ = units.record_result(
        task_id,
        lease,
        unit.unit_seq,
        "artifact.write",
        "artifact.write",
        &format!("{}", path.display()),
        "system",
        now,
    );
    let _ = units.finish_unit(task_id, lease, unit.unit_seq, ExecStatus::Done, 0, 1, now);
    Ok(path)
}
