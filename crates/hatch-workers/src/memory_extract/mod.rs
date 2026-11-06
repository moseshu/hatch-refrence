//! `memory_extract`: pull durable facts out of conversations into memory.
//!
//! The turn loop never writes memory by itself — the model may forget to
//! call `memory_add`. This worker scans sessions that gained messages since
//! its last run, asks the model (MemoryFlush workload) to extract durable
//! facts / preferences / decisions / commitments, and writes them as claims
//! with `inferred = true` and a `session:<id> seq <a>-<b>` source.
//!
//! Write policy (same philosophy as `memory_flush`): a claim that does not
//! conflict with anything current is written automatically; one that is
//! similar (≥ 0.75) to an existing claim is only proposed — the user
//! decides. Nothing is ever deleted or silently overwritten. Secrets are
//! rejected by the store's validation and counted, never written.
//!
//! Progress is a per-session watermark in the `meta` table
//! (`memory_extract:seq:<session>`). A session is only marked done after
//! its claims were handled; a failed session keeps its old watermark and is
//! retried next run. Re-running is safe: `MemoryStore::add` dedups exact
//! duplicates, and similar ones become proposals again.

pub mod extract;

use crate::memory_flush::similarity::similarity;
use crate::outputs::{deliver, WorkerOutputs};
use crate::{make_router, record_usage, RouterFactory};
use extract::{extract_claims, sessions_with_new, transcript};
use hatch_core::time::Timestamp;
use hatch_core::{CancelToken, Error, Result};
use hatch_llm::Router;
use hatch_memory::{MemoryStore, NewClaim};
use hatch_policy::AttentionPolicy;
use hatch_scheduler::{JobContext, JobHandler, JobOutcome};
use hatch_store::{params, Db, DbResultExt, OptionalExtension};

pub const KIND: &str = "memory_extract";
/// Same threshold memory_flush uses for "propose, don't auto-write".
pub const PROPOSE_THRESHOLD: f64 = 0.75;
const MAX_CLAIMS_PER_SESSION: usize = 20;

pub struct MemoryExtractWorker {
    pub router: Option<RouterFactory>,
    pub attention: AttentionPolicy,
}

/// An extracted claim held back because it may duplicate a current one.
#[derive(Clone, Debug, Default)]
pub struct Proposal {
    pub existing_id: String,
    pub existing_text: String,
    pub extracted_text: String,
    pub similarity: f64,
}

#[derive(Clone, Debug, Default)]
pub struct ExtractReport {
    pub sessions: usize,
    pub added: Vec<(String, String)>,
    pub proposals: Vec<Proposal>,
    pub skipped_secrets: usize,
    pub skipped_long: usize,
    /// e.g. "没有配置模型" — work skipped, not failed.
    pub note: Option<String>,
}

fn watermark_key(session: &str) -> String {
    format!("memory_extract:seq:{session}")
}

fn watermark(db: &Db, session: &str) -> Result<i64> {
    Ok(db.meta(&watermark_key(session))?.and_then(|v| v.parse().ok()).unwrap_or(0))
}

impl MemoryExtractWorker {
    /// Extract from one session (used by the manual `hatch memory extract`
    /// command). Builds a fresh router; without a model the report just
    /// notes the skip.
    pub fn extract_session(&self, db: &Db, session: &str, now: &Timestamp, cancel: &CancelToken) -> Result<ExtractReport> {
        if db
            .conn()
            .query_row("SELECT 1 FROM sessions WHERE id = ?1", params![session], |_| Ok(()))
            .optional()
            .db()?
            .is_none()
        {
            return Err(Error::NotFound(format!("session {session}")));
        }
        let mut report = ExtractReport::default();
        match make_router(&self.router) {
            Err(why) => {
                report.note = Some(why);
                return Ok(report);
            }
            Ok(router) => {
                self.process_session(db, &router, session, now, cancel, &mut report)?;
                record_usage(db, &router, now)?;
            }
        }
        Ok(report)
    }

    /// Extract from every session with new messages (used by the scheduled
    /// job and by `hatch memory extract` without `--session`).
    pub fn extract_all(&self, db: &Db, now: &Timestamp, cancel: &CancelToken) -> Result<ExtractReport> {
        let mut report = ExtractReport::default();
        let router = match make_router(&self.router) {
            Err(why) => {
                report.note = Some(why);
                return Ok(report);
            }
            Ok(r) => r,
        };
        let sessions = sessions_with_new(db, &|id| watermark(db, id).unwrap_or(0))?;
        for (id, _) in &sessions {
            cancel.check()?;
            self.process_session(db, &router, id, now, cancel, &mut report)?;
        }
        record_usage(db, &router, now)?;
        Ok(report)
    }

    fn process_session(
        &self,
        db: &Db,
        router: &Router,
        session: &str,
        now: &Timestamp,
        cancel: &CancelToken,
        report: &mut ExtractReport,
    ) -> Result<()> {
        let since = watermark(db, session)?;
        let text = transcript(db, session, since)?;
        let max: i64 = db
            .conn()
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM session_messages WHERE session_id = ?1",
                params![session],
                |r| r.get(0),
            )
            .db()?;
        if max <= since {
            return Ok(());
        }
        let claims = match extract_claims(router, &text, cancel) {
            Ok(c) => c,
            Err(e) => {
                // Model or parse failure: leave the watermark alone so the
                // next run retries this session.
                report.note = Some(format!("会话 {session} 提炼失败：{e}"));
                return Ok(());
            }
        };
        let mem = MemoryStore::new(db);
        let mut known: Vec<(String, String)> = mem
            .current(now)?
            .into_iter()
            .map(|c| (c.id, c.text))
            .collect();
        let source = format!("session:{session} seq {}-{max}", since + 1);
        for c in claims.into_iter().take(MAX_CLAIMS_PER_SESSION) {
            cancel.check()?;
            if c.text.chars().count() > 2000 {
                report.skipped_long += 1;
                continue;
            }
            let best = known
                .iter()
                .map(|(id, t)| (id.clone(), t.clone(), similarity(&c.text, t)))
                .max_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
            if let Some((id, text, s)) = best.filter(|(_, _, s)| *s >= PROPOSE_THRESHOLD) {
                report.proposals.push(Proposal { existing_id: id, existing_text: text, extracted_text: c.text, similarity: (s * 100.0).round() / 100.0 });
                continue;
            }
            let claim = NewClaim {
                subject: c.subject,
                text: c.text.clone(),
                tags: c.tags,
                source: source.clone(),
                trust: hatch_core::Trust::User,
                confidence: c.confidence,
                valid_until: None,
                inferred: true,
                salience: c.salience,
            };
            match mem.add(&claim, now) {
                Ok(outcome) => {
                    let id = outcome.id().to_string();
                    if !report.added.iter().any(|(i, _)| *i == id) {
                        report.added.push((id.clone(), c.text.clone()));
                    }
                    known.push((id, c.text));
                }
                Err(Error::SecretRejected(_)) => report.skipped_secrets += 1,
                Err(e) => return Err(e),
            }
        }
        db.set_meta(&watermark_key(session), &max.to_string())?;
        report.sessions += 1;
        Ok(())
    }
}

pub fn render(r: &ExtractReport) -> String {
    let mut s = String::from("# 记忆提炼报告\n");
    if let Some(n) = &r.note {
        s.push_str(&format!("\n（{n}）\n"));
    }
    s.push_str(&format!("\n## 已提炼（{} 个会话，新增 {} 条）\n", r.sessions, r.added.len()));
    if r.added.is_empty() {
        s.push_str("- 无新增\n");
    }
    for (id, text) in &r.added {
        s.push_str(&format!("- [{id}] {text}\n"));
    }
    s.push_str(&format!("\n## 待你决定（与现有记忆相似，{} 组，未写入）\n", r.proposals.len()));
    if r.proposals.is_empty() {
        s.push_str("- 无\n");
    }
    for p in &r.proposals {
        s.push_str(&format!(
            "- 现有 [{}] {}\n  新提炼 {}\n  相似度 {:.2}。如确认是同一事实，用 `hatch memory correct {}` 纠正旧记录；如确认是新事实，用 `hatch memory add` 手动记下。\n",
            p.existing_id, p.existing_text, p.extracted_text, p.similarity, p.existing_id
        ));
    }
    if r.skipped_secrets > 0 || r.skipped_long > 0 {
        s.push_str(&format!("\n跳过：凭据类 {} 条，过长 {} 条。\n", r.skipped_secrets, r.skipped_long));
    }
    s
}

impl JobHandler for MemoryExtractWorker {
    fn run(&self, ctx: &JobContext) -> Result<JobOutcome> {
        let r = self.extract_all(ctx.db, &ctx.now, &ctx.cancel)?;
        let summary = format!("扫描 {} 个会话，新增 {} 条，待决定 {} 组", r.sessions, r.added.len(), r.proposals.len());
        if !r.added.is_empty() || !r.proposals.is_empty() {
            WorkerOutputs::new(ctx.db).add("memory_extract", "记忆提炼报告", &render(&r), &ctx.now)?;
            deliver(ctx.db, &self.attention, ctx.job, &format!("记忆提炼：{summary}"), &format!("worker:{}", ctx.run_id), &ctx.now)?;
        }
        ctx.db.audit("memory_extract", "report", &summary)?;
        Ok(JobOutcome::Done { summary })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatch_core::time::now;
    use hatch_llm::scripted::ScriptedModel;
    use hatch_llm::ChatResponse;
    use hatch_scheduler::{Engine, JobStore, NewJob, RunStatus};
    use std::sync::Arc;

    fn seed_db() -> (Db, String) {
        let db = Db::open_in_memory().unwrap();
        let t = now();
        let id = "ses_x".to_string();
        db.conn()
            .execute(
                "INSERT INTO sessions(id, created_at, title) VALUES(?1, ?2, 't')",
                params![id, hatch_core::time::to_rfc3339(&t)],
            )
            .unwrap();
        for (i, (role, content)) in [
            ("user", "以后回复邮件请用中文，语气简洁"),
            ("assistant", "好的"),
            ("user", "周三下午别给我排会"),
        ]
        .iter()
        .enumerate()
        {
            db.conn()
                .execute(
                    "INSERT INTO session_messages(session_id, seq, role, content, meta, compacted, created_at) VALUES(?1,?2,?3,?4,'{}',0,?5)",
                    params![id, i as i64 + 1, role, content, hatch_core::time::to_rfc3339(&t)],
                )
                .unwrap();
        }
        (db, id)
    }

    fn worker_with(body: &str) -> (MemoryExtractWorker, Arc<ScriptedModel>) {
        let model = Arc::new(ScriptedModel::ok("m", vec![ChatResponse::text(body)]));
        let m = model.clone();
        let factory: RouterFactory = Arc::new(move || Ok(Router::single(m.clone())));
        let w = MemoryExtractWorker {
            router: Some(factory),
            attention: AttentionPolicy { tz: chrono_tz::Asia::Shanghai, ..Default::default() },
        };
        (w, model)
    }

    const BODY: &str = "{\"claims\":[\
        {\"subject\":\"preference:reply\",\"text\":\"回复邮件请用中文，语气简洁\",\"tags\":[\"preference\"],\"confidence\":0.8,\"salience\":0.8},\
        {\"subject\":\"preference:meeting\",\"text\":\"周三下午不安排会议\",\"tags\":[\"preference\"],\"confidence\":0.9}]}";

    #[test]
    fn extracts_writes_and_is_idempotent() {
        let (db, id) = seed_db();
        let t = now();
        let (w, _) = worker_with(BODY);
        let r = w.extract_session(&db, &id, &t, &CancelToken::new()).unwrap();
        assert_eq!(r.added.len(), 2, "{r:?}");
        assert!(r.proposals.is_empty());
        let mem = MemoryStore::new(&db);
        assert_eq!(mem.current(&t).unwrap().len(), 2);
        assert!(mem.current(&t).unwrap().iter().all(|c| c.inferred && c.source.starts_with("session:ses_x")));
        let saliences: Vec<f64> = mem.current(&t).unwrap().iter().map(|c| c.salience).collect();
        assert!(saliences.contains(&0.8), "model-scored salience is stored: {saliences:?}");
        assert!(saliences.contains(&0.5), "missing salience defaults to neutral: {saliences:?}");
        // Second run: watermark hides the session, nothing is written twice.
        let r2 = w.extract_session(&db, &id, &t, &CancelToken::new()).unwrap();
        assert_eq!(r2.added.len(), 0);
        assert_eq!(mem.current(&t).unwrap().len(), 2);
        assert_eq!(db.meta(&watermark_key(&id)).unwrap().as_deref(), Some("3"));
    }

    #[test]
    fn similar_claim_is_proposed_not_written() {
        let (db, id) = seed_db();
        let t = now();
        let mem = MemoryStore::new(&db);
        mem.add(&NewClaim::stated("preference:reply", "回复邮件请用中文，语气要简洁", "user:chat").with_tags(&["preference"]), &t)
            .unwrap();
        let (w, _) = worker_with(BODY);
        let r = w.extract_session(&db, &id, &t, &CancelToken::new()).unwrap();
        assert_eq!(r.added.len(), 1, "only the meeting claim is new: {r:?}");
        assert_eq!(r.proposals.len(), 1);
        assert!(r.proposals[0].similarity >= 0.75);
        assert_eq!(mem.current(&t).unwrap().len(), 2, "proposal wrote nothing");
    }

    #[test]
    fn secret_claims_are_skipped_never_written() {
        let (db, id) = seed_db();
        let t = now();
        let body = "{\"claims\":[{\"subject\":\"account\",\"text\":\"126 邮箱授权码：ABCD1234XYZ\",\"tags\":[],\"confidence\":0.9}]}";
        let (w, _) = worker_with(body);
        let r = w.extract_session(&db, &id, &t, &CancelToken::new()).unwrap();
        assert_eq!(r.added.len(), 0);
        assert_eq!(r.skipped_secrets, 1);
        assert!(MemoryStore::new(&db).current(&t).unwrap().is_empty());
        assert!(!render(&r).contains("ABCD1234XYZ"), "report must not echo the secret");
    }

    #[test]
    fn no_model_skips_gracefully_and_unknown_session_errors() {
        let (db, id) = seed_db();
        let t = now();
        let w = MemoryExtractWorker { router: None, attention: AttentionPolicy { tz: chrono_tz::Asia::Shanghai, ..Default::default() } };
        let r = w.extract_session(&db, &id, &t, &CancelToken::new()).unwrap();
        assert_eq!(r.added.len(), 0);
        assert!(r.note.as_deref().unwrap_or("").contains("没有配置模型"));
        assert!(w.extract_session(&db, "ses_nope", &t, &CancelToken::new()).is_err());
    }

    #[test]
    fn scheduled_run_delivers_report() {
        let (db, id) = seed_db();
        let t = now();
        let (w, _) = worker_with(BODY);
        let e = Engine::new(&db, chrono_tz::Asia::Shanghai).register(KIND, Arc::new(w));
        JobStore::new(&db).add(&NewJob::new("extract", KIND, "daily 04:00"), &t, &chrono_tz::Asia::Shanghai).unwrap();
        let r = e.run_now("extract", &t, &CancelToken::new()).unwrap();
        assert_eq!(r.status, RunStatus::Ok, "{r:?}");
        let out = crate::WorkerOutputs::new(&db).unread().unwrap();
        assert_eq!(out.len(), 1);
        assert!(out[0].content.contains("记忆提炼报告") && out[0].content.contains("回复邮件请用中文"));
        assert_eq!(db.meta(&watermark_key(&id)).unwrap().as_deref(), Some("3"));
    }
}
