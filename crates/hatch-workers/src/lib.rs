//! `hatch-workers` — work the scheduler runs while nobody is chatting.
//!
//! | Kind | What it does | Model |
//! |---|---|---|
//! | [`brief::KIND`] `daily_brief` | today's and tomorrow's mail, calendar, conflicts, approvals, reminders | Brief (phrasing only; every item cites a fact) |
//! | [`triage_sync::KIND`] `triage_sync` | runs mail triage on a schedule | Triage / Draft |
//! | [`memory_flush::KIND`] `memory_flush` | merges near-duplicate claims, reports expired ones, proposes the rest | MemoryFlush (proposals only) |
//! | [`memory_extract::KIND`] `memory_extract` | extracts durable facts from new session turns into memory | MemoryFlush (extraction; conflicts only proposed) |
//! | [`memory_synthesis::KIND`] `memory_synthesis` | summarises the claim bank into one "current understanding" document | MemoryFlush (summary; read-only, never writes claims) |
//!
//! Workers never widen permissions: external actions go through a scoped
//! gatekeeper and the approval queue. Their output is stored for the next
//! chat ([`outputs`]) and announced according to the job's delivery setting.

pub mod brief;
pub mod memory_extract;
pub mod memory_flush;
pub mod memory_synthesis;
pub mod outputs;
pub mod seed;
pub mod triage_sync;

pub use brief::{BriefStore, BriefWorker, StoredBrief};
pub use memory_extract::MemoryExtractWorker;
pub use memory_flush::MemoryFlushWorker;
pub use memory_synthesis::MemorySynthesisWorker;
pub use outputs::{deliver, handoff_lines, WorkerOutput, WorkerOutputs};
pub use triage_sync::TriageSyncWorker;

use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_llm::Router;
use hatch_store::{params, Db, DbResultExt};
use std::sync::Arc;

/// Builds a fresh router (with a fresh call budget) for one job run.
pub type RouterFactory = Arc<dyn Fn() -> hatch_core::Result<Router> + Send + Sync>;

/// Build a router if a factory is configured; `Err` carries why there is none.
pub fn make_router(f: &Option<RouterFactory>) -> std::result::Result<Router, String> {
    match f {
        None => Err("没有配置模型".into()),
        Some(f) => f().map_err(|e| format!("模型不可用：{e}")),
    }
}

/// Persist a run's token usage (tokens only — no prices are invented).
pub fn record_usage(db: &Db, router: &Router, now: &Timestamp) -> hatch_core::Result<()> {
    for r in router.take_records() {
        db.conn()
            .execute(
                "INSERT INTO usage_log(ts, workload, model, prompt_tokens, completion_tokens, cached_tokens) VALUES(?1,?2,?3,?4,?5,?6)",
                params![
                    to_rfc3339(now),
                    r.workload.as_str(),
                    r.model,
                    r.usage.prompt_tokens as i64,
                    r.usage.completion_tokens as i64,
                    r.usage.cached_tokens as i64
                ],
            )
            .db()?;
    }
    Ok(())
}

/// All built-in job kinds.
pub const KINDS: [&str; 5] = [brief::KIND, triage_sync::KIND, memory_flush::KIND, memory_extract::KIND, memory_synthesis::KIND];
