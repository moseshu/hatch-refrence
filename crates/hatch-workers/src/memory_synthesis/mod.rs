//! `memory_synthesis` (P3): summarise the claim bank into one "current
//! understanding" document, once a night.
//!
//! Runs at 02:30 — after midnight while the user is asleep, and clear of
//! the other two nightly model jobs (03:15 `memory_flush`, 04:00
//! `memory_extract`) so three model calls never contend. The one-night
//! lag on that night's extraction is fine: the document is a stable
//! overview, not the retrieval path — fresh claims still surface directly
//! through BM25 recall the next day.
//!
//! The document is a **read-only summary of the claims**: it never becomes
//! a claim, never changes salience, and is overwritten in full each run
//! (only the latest copy is kept). A failed run keeps the previous
//! document — the store is only written after a successful model call
//! and render, so a half-written document is impossible. The agent
//! injects the document at the start of every turn (stable prefix, right
//! after the behaviour spec), so the model always starts from an
//! up-to-date picture of the user.

pub mod synthesize;

use crate::{make_router, record_usage, RouterFactory};
use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::{CancelToken, Result};
use hatch_memory::{SynthesisDoc};
use hatch_scheduler::{JobContext, JobHandler, JobOutcome};
use hatch_store::Db;
use synthesize::{gather_input, load, render, store, synthesize_sections};

pub const KIND: &str = "memory_synthesis";
/// Without `--force`, do not re-synthesise within this long of the last
/// one. The scheduled job runs once a day; the guard stops duplicate
/// model calls when the command is run twice by hand.
pub const MIN_INTERVAL: chrono::Duration = chrono::Duration::hours(12);

pub struct MemorySynthesisWorker {
    pub router: Option<RouterFactory>,
}

#[derive(Clone, Debug, Default)]
pub struct SynthReport {
    /// A new document was stored.
    pub generated: bool,
    /// Skipped by the recency guard (run again with `--force`).
    pub skipped_recent: bool,
    /// Claims that went into the document (0 when nothing was generated).
    pub source_claims: usize,
    /// e.g. "没有配置模型" — work skipped, not failed.
    pub note: Option<String>,
}

impl MemorySynthesisWorker {
    /// Generate (or skip) one synthesis. Never fails the run for a missing
    /// model or a model error — the previous document stays in place and
    /// the reason lands in `report.note`.
    pub fn synthesize(&self, db: &Db, now: &Timestamp, cancel: &CancelToken, force: bool) -> Result<SynthReport> {
        let mut report = SynthReport::default();
        if !force {
            if let Some(prev) = load(db)? {
                if *now - prev.generated_at < MIN_INTERVAL {
                    report.skipped_recent = true;
                    report.note = Some(format!(
                        "已在 {} 生成过，用 `hatch memory synthesize --force` 重新生成",
                        to_rfc3339(&prev.generated_at)
                    ));
                    return Ok(report);
                }
            }
        }
        let input = gather_input(db, now)?;
        if input.source_claims == 0 {
            report.note = Some("没有记忆可综合".into());
            return Ok(report);
        }
        let router = match make_router(&self.router) {
            Err(why) => {
                report.note = Some(why);
                return Ok(report);
            }
            Ok(r) => r,
        };
        let sections = match synthesize_sections(&router, &input, cancel) {
            Ok(s) => s,
            Err(e) => {
                // Model or parse failure: keep the previous document.
                report.note = Some(format!("综合失败：{e}，保留上一份"));
                return Ok(report);
            }
        };
        let doc = SynthesisDoc { content: render(&sections), generated_at: *now, source_claims: input.source_claims };
        store(db, &doc)?;
        record_usage(db, &router, now)?;
        report.generated = true;
        report.source_claims = input.source_claims;
        Ok(report)
    }
}

impl JobHandler for MemorySynthesisWorker {
    fn run(&self, ctx: &JobContext) -> Result<JobOutcome> {
        let r = self.synthesize(ctx.db, &ctx.now, &ctx.cancel, false)?;
        let summary = if r.generated {
            format!("已综合 {} 条记忆", r.source_claims)
        } else {
            format!("未生成（{}）", r.note.as_deref().unwrap_or("跳过"))
        };
        ctx.db.audit("memory_synthesis", "report", &summary)?;
        Ok(JobOutcome::Done { summary })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatch_core::time::now;
    use hatch_llm::scripted::ScriptedModel;
    use hatch_llm::ChatResponse;
    use hatch_memory::{MemoryStore, NewClaim};
    use hatch_scheduler::{Engine, JobStore, NewJob, RunStatus};
    use std::sync::Arc;

    const BODY1: &str = "{\"profile\":\"注重效率的工程师\",\"preferences\":[\"回复邮件用中文\"],\"ongoing\":[],\"facts\":[\"周三下午不安排会议\"]}";
    const BODY2: &str = "{\"profile\":\"喜欢跑步的工程师\",\"preferences\":[],\"ongoing\":[\"准备周五的预算评审\"],\"facts\":[]}";

    fn seed_db() -> Db {
        let db = Db::open_in_memory().unwrap();
        let t = now();
        let m = MemoryStore::new(&db);
        m.add(&NewClaim::stated("preference:reply", "回复邮件请用中文，语气简洁", "user:chat").with_salience(0.9), &t).unwrap();
        m.add(&NewClaim::stated("preference:meeting", "周三下午不安排会议", "user:chat").with_salience(0.5), &t).unwrap();
        db
    }

    fn worker_with(body: &str) -> (MemorySynthesisWorker, Arc<ScriptedModel>) {
        let model = Arc::new(ScriptedModel::ok("m", vec![ChatResponse::text(body)]));
        let m = model.clone();
        let factory: RouterFactory = Arc::new(move || Ok(hatch_llm::Router::single(m.clone())));
        (MemorySynthesisWorker { router: Some(factory) }, model)
    }

    #[test]
    fn synthesizes_stores_and_overwrites_with_force() {
        let db = seed_db();
        let t = now();
        let (w1, _) = worker_with(BODY1);
        let r1 = w1.synthesize(&db, &t, &CancelToken::new(), false).unwrap();
        assert!(r1.generated && r1.source_claims == 2, "{r1:?}");
        let doc = load(&db).unwrap().unwrap();
        assert!(doc.content.contains("注重效率的工程师") && doc.content.contains("回复邮件用中文"));
        assert_eq!(doc.source_claims, 2);
        // Second run without --force is skipped by the recency guard.
        let r2 = w1.synthesize(&db, &t, &CancelToken::new(), false).unwrap();
        assert!(!r2.generated && r2.skipped_recent, "{r2:?}");
        // --force overwrites with the new body.
        let (w2, _) = worker_with(BODY2);
        let r3 = w2.synthesize(&db, &t, &CancelToken::new(), true).unwrap();
        assert!(r3.generated, "{r3:?}");
        let doc = load(&db).unwrap().unwrap();
        assert!(doc.content.contains("喜欢跑步的工程师") && doc.content.contains("准备周五的预算评审"));
        assert!(!doc.content.contains("注重效率的工程师"), "only the latest document is kept");
    }

    #[test]
    fn model_failure_keeps_previous_doc() {
        let db = seed_db();
        let t = now();
        let (w1, _) = worker_with(BODY1);
        w1.synthesize(&db, &t, &CancelToken::new(), false).unwrap();
        let before = load(&db).unwrap().unwrap().content;
        let bad = Arc::new(ScriptedModel::ok("m", vec![ChatResponse::text("not json")]));
        let b = bad.clone();
        let factory: RouterFactory = Arc::new(move || Ok(hatch_llm::Router::single(b.clone())));
        let w2 = MemorySynthesisWorker { router: Some(factory) };
        let r = w2.synthesize(&db, &t, &CancelToken::new(), true).unwrap();
        assert!(!r.generated, "{r:?}");
        assert!(r.note.as_deref().unwrap_or("").contains("保留上一份"), "{r:?}");
        assert_eq!(load(&db).unwrap().unwrap().content, before, "failed run writes nothing");
    }

    #[test]
    fn no_model_and_empty_memory_skip_gracefully() {
        let db = seed_db();
        let t = now();
        let w = MemorySynthesisWorker { router: None };
        let r = w.synthesize(&db, &t, &CancelToken::new(), false).unwrap();
        assert!(!r.generated);
        assert!(r.note.as_deref().unwrap_or("").contains("没有配置模型"));
        assert!(load(&db).unwrap().is_none(), "skipped run writes nothing");
        let empty = Db::open_in_memory().unwrap();
        let (w2, model) = worker_with(BODY1);
        let r2 = w2.synthesize(&empty, &t, &CancelToken::new(), false).unwrap();
        assert!(!r2.generated);
        assert!(r2.note.as_deref().unwrap_or("").contains("没有记忆可综合"));
        assert_eq!(model.requests().len(), 0, "no claims means no model call");
    }

    #[test]
    fn scheduled_run_stores_the_document() {
        let db = seed_db();
        let t = now();
        let (w, _) = worker_with(BODY1);
        let e = Engine::new(&db, chrono_tz::Asia::Shanghai).register(KIND, Arc::new(w));
        JobStore::new(&db).add(&NewJob::new("synth", KIND, "daily 02:30"), &t, &chrono_tz::Asia::Shanghai).unwrap();
        let r = e.run_now("synth", &t, &CancelToken::new()).unwrap();
        assert_eq!(r.status, RunStatus::Ok, "{r:?}");
        assert!(load(&db).unwrap().unwrap().content.contains("注重效率的工程师"));
    }
}
