//! `MemoryStore` — the facade over the bank and the index.
//!
//! Callers use this type only. The bank (`bank.rs`) owns claim writes and
//! reads; the index (`index.rs`) owns ranked retrieval. `search` reads the
//! persistent index, self-healing first; if the index is unreadable it
//! rebuilds once, and if that also fails it falls back to the legacy
//! in-memory BM25 build — retrieval never fails because of the index.

use crate::bank::Bank;
use crate::claim::{AddOutcome, Claim, Explanation, ForgetReport, NewClaim, FORGET_CAVEAT};
use crate::index::{doc_text, IndexReport, MemoryIndex};
use crate::people::People;
use crate::ranking::{rescore, RankingConfig};
use crate::search::Bm25Index;
use hatch_core::time::Timestamp;
use hatch_core::Result;
use hatch_store::Db;

/// Candidates rescored per search — same cap as the index returns (200).
const RESCORE_CANDIDATES: usize = 200;

pub struct MemoryStore<'a> {
    db: &'a Db,
    bank: Bank<'a>,
    index: MemoryIndex<'a>,
    ranking: RankingConfig,
}

impl<'a> MemoryStore<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self::with_ranking(db, RankingConfig::default())
    }

    pub fn with_ranking(db: &'a Db, ranking: RankingConfig) -> Self {
        MemoryStore {
            db,
            bank: Bank::new(db),
            index: MemoryIndex::new(db),
            ranking,
        }
    }

    pub fn people(&self) -> People<'a> {
        People::new(self.db)
    }

    pub fn add(&self, c: &NewClaim, now: &Timestamp) -> Result<AddOutcome> {
        self.bank.add(c, now)
    }

    pub fn correct(&self, old_id: &str, new: &NewClaim, now: &Timestamp) -> Result<String> {
        self.bank.correct(old_id, new, now)
    }

    pub fn get(&self, id: &str) -> Result<Option<Claim>> {
        self.bank.get(id)
    }

    /// All current claims.
    pub fn current(&self, now: &Timestamp) -> Result<Vec<Claim>> {
        self.bank.current(now)
    }

    pub fn current_for_subject(&self, subject: &str, now: &Timestamp) -> Result<Vec<Claim>> {
        self.bank.current_for_subject(subject, now)
    }

    /// Current claims carrying any of `tags` (e.g. `preference`, `style`).
    pub fn with_tags(&self, tags: &[&str], now: &Timestamp) -> Result<Vec<Claim>> {
        self.bank.with_tags(tags, now)
    }

    /// Full provenance: older versions and newer versions. Cycle-safe.
    pub fn explain(&self, id: &str) -> Result<Explanation> {
        self.bank.explain(id)
    }

    /// Forget a claim and every version in its chain.
    pub fn forget_claim(&self, id: &str) -> Result<ForgetReport> {
        self.bank.forget_claim(id)
    }

    /// Forget everything about a subject (all versions), and the person page if it is `person:<email>`.
    pub fn forget_subject(&self, subject: &str) -> Result<ForgetReport> {
        let ids = self.bank.forget_subject_claims(subject)?;
        let person_removed = match subject.strip_prefix("person:") {
            Some(email) => self.people().remove(email)?,
            None => false,
        };
        self.db.audit(
            "memory",
            "forget",
            &format!("subject_claims={} person={person_removed}", ids.join(",")),
        )?;
        Ok(ForgetReport {
            claims_removed: ids.len(),
            person_removed,
            caveat: FORGET_CAVEAT.into(),
        })
    }

    /// Merge a near-duplicate: mark `old_id` as replaced by the existing
    /// current claim `keep_id`. Nothing is deleted — `explain` shows the link.
    pub fn merge_into(&self, old_id: &str, keep_id: &str, now: &Timestamp) -> Result<()> {
        self.bank.merge_into(old_id, keep_id, now)
    }

    /// Claims whose `valid_until` has passed and that were never replaced.
    pub fn expired(&self, now: &Timestamp) -> Result<Vec<Claim>> {
        self.bank.expired(now)
    }

    /// Self-healing index check, for `memory_flush` and `hatch memory reindex`.
    pub fn verify_index(&self) -> Result<IndexReport> {
        self.index.verify()
    }

    /// Force a full index rebuild.
    pub fn reindex(&self) -> Result<IndexReport> {
        self.index.rebuild()
    }

    /// Ranked search over current claims, via the persistent index.
    pub fn search(&self, query: &str, k: usize, now: &Timestamp) -> Result<Vec<(Claim, f64)>> {
        match self.search_indexed(query, k, now) {
            Ok(hits) => Ok(hits),
            Err(first) => {
                // One full rebuild, then retry; if the index is still
                // unusable, fall back to the legacy in-memory build.
                // Retrieval never fails because of the index.
                match self
                    .index
                    .rebuild()
                    .and_then(|_| self.search_indexed(query, k, now))
                {
                    Ok(hits) => {
                        let _ = self.db.audit(
                            "memory",
                            "index_rebuilt",
                            &format!("persistent index unreadable, rebuilt once: {first}"),
                        );
                        Ok(hits)
                    }
                    Err(_) => {
                        let _ = self.db.audit(
                            "memory",
                            "index_fallback",
                            &format!("persistent index unusable, searched in-memory: {first}"),
                        );
                        self.search_legacy(query, k, now)
                    }
                }
            }
        }
    }

    /// Record that claims were injected into a turn's context (P2).
    /// Best-effort: a failure here must never fail the caller's turn.
    pub fn mark_used(&self, ids: &[String], now: &Timestamp) -> Result<()> {
        self.bank.mark_used(ids, now)
    }

    /// Hand-tune a claim's ranking weight (P2).
    pub fn set_salience(&self, id: &str, salience: f64) -> Result<()> {
        self.bank.set_salience(id, salience)
    }

    fn search_indexed(&self, query: &str, k: usize, now: &Timestamp) -> Result<Vec<(Claim, f64)>> {
        let mut out = Vec::new();
        for (id, score) in self.index.query(query)? {
            let Some(c) = self.bank.get(&id)? else {
                continue;
            };
            if !c.is_current(now) {
                continue;
            }
            out.push((c, score));
        }
        Ok(rescore(out, k, now, self.ranking))
    }

    /// The pre-split path: build a BM25 index in memory from current
    /// claims. Kept as the fallback when the persistent index is unusable,
    /// and as the oracle for the comparison test below. It pulls the same
    /// candidate pool as the index path and rescores identically.
    fn search_legacy(&self, query: &str, k: usize, now: &Timestamp) -> Result<Vec<(Claim, f64)>> {
        let claims = self.bank.current(now)?;
        let idx = Bm25Index::build(
            claims
                .iter()
                .map(|c| (c.id.as_str(), doc_text(&c.subject, &c.text, &c.tags))),
        );
        let hits = idx.query(query, RESCORE_CANDIDATES);
        let hits: Vec<(Claim, f64)> = hits
            .into_iter()
            .filter_map(|(id, s)| claims.iter().find(|c| c.id == id).map(|c| (c.clone(), s)))
            .collect();
        Ok(rescore(hits, k, now, self.ranking))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use hatch_core::{Error, Trust};

    fn setup() -> Db {
        Db::open_in_memory().unwrap()
    }

    #[test]
    fn add_validates_and_dedups() {
        let db = setup();
        let m = MemoryStore::new(&db);
        let now = hatch_core::time::now();
        let c = NewClaim::stated("preference:reply", "回复要简短，不要客套", "user:chat");
        let a = m.add(&c, &now).unwrap();
        assert!(matches!(a, AddOutcome::Added(_)));
        let b = m
            .add(
                &NewClaim::stated("preference:reply", " 回复要简短，不要客套 ", "user:chat"),
                &now,
            )
            .unwrap();
        assert_eq!(b, AddOutcome::Duplicate(a.id().to_string()));
        assert!(m.add(&NewClaim::stated("x", "y", " "), &now).is_err());
        let mut bad = NewClaim::stated("x", "y", "s");
        bad.confidence = 1.5;
        assert!(m.add(&bad, &now).is_err());
    }

    #[test]
    fn secrets_are_rejected() {
        let db = setup();
        let m = MemoryStore::new(&db);
        let now = hatch_core::time::now();
        let e = m
            .add(
                &NewClaim::stated("account", "126 邮箱授权码：ABCD1234XYZ", "user:chat"),
                &now,
            )
            .unwrap_err();
        assert!(matches!(e, Error::SecretRejected(_)));
        assert!(!e.to_string().contains("ABCD1234XYZ"));
        let e = m
            .add(
                &NewClaim::stated("bank", "卡号 4111111111111111", "user:chat"),
                &now,
            )
            .unwrap_err();
        assert!(matches!(e, Error::SecretRejected(_)));
        assert_eq!(m.current(&now).unwrap().len(), 0);
    }

    #[test]
    fn confidence_caps() {
        let db = setup();
        let m = MemoryStore::new(&db);
        let now = hatch_core::time::now();
        let id = m
            .add(
                &NewClaim::stated("u", "likes tea", "mail:1").with_trust(Trust::Untrusted, 0.95),
                &now,
            )
            .unwrap();
        assert_eq!(m.get(id.id()).unwrap().unwrap().confidence, 0.5);
        let id = m
            .add(
                &NewClaim::stated("u", "works late", "behaviour").inferred(),
                &now,
            )
            .unwrap();
        assert_eq!(m.get(id.id()).unwrap().unwrap().confidence, 0.7);
    }

    #[test]
    fn correction_chain_and_explain() {
        let db = setup();
        let m = MemoryStore::new(&db);
        let now = hatch_core::time::now();
        let a = m
            .add(
                &NewClaim::stated("person:li@x.com", "李雷在 A 公司", "user:chat"),
                &now,
            )
            .unwrap();
        let b = m
            .correct(
                a.id(),
                &NewClaim::stated("person:li@x.com", "李雷跳槽到 B 公司", "user:chat"),
                &now,
            )
            .unwrap();
        let c = m
            .correct(
                &b,
                &NewClaim::stated("person:li@x.com", "李雷在 B 公司任总监", "user:chat"),
                &now,
            )
            .unwrap();
        assert!(m
            .correct(a.id(), &NewClaim::stated("s", "t", "u"), &now)
            .is_err());
        let ex = m.explain(&c).unwrap();
        assert_eq!(ex.history.len(), 2);
        assert_eq!(ex.history[0].id, b);
        let ex = m.explain(a.id()).unwrap();
        assert_eq!(ex.successors.len(), 2);
        let cur = m.current_for_subject("person:li@x.com", &now).unwrap();
        assert_eq!(cur.len(), 1);
        assert_eq!(cur[0].id, c);
        assert!(m.explain("nope").is_err());
        let hits = m.search("李雷 公司", 5, &now).unwrap();
        assert_eq!(hits.len(), 1, "search returns only the current version");
    }

    #[test]
    fn expiry_hides_claims() {
        let db = setup();
        let m = MemoryStore::new(&db);
        let now = hatch_core::time::now();
        let mut c = NewClaim::stated("trip", "下周在上海出差", "user:chat");
        c.valid_until = Some(now + Duration::days(1));
        m.add(&c, &now).unwrap();
        assert_eq!(m.search("出差", 3, &now).unwrap().len(), 1);
        assert_eq!(
            m.search("出差", 3, &(now + Duration::days(2)))
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn forget_removes_whole_chain() {
        let db = setup();
        let m = MemoryStore::new(&db);
        let now = hatch_core::time::now();
        let a = m.add(&NewClaim::stated("s", "v1", "u"), &now).unwrap();
        let b = m
            .correct(a.id(), &NewClaim::stated("s", "v2", "u"), &now)
            .unwrap();
        m.add(&NewClaim::stated("other", "keep", "u"), &now)
            .unwrap();
        let r = m.forget_claim(&b).unwrap();
        assert_eq!(r.claims_removed, 2);
        assert!(r.caveat.contains("无法被撤回"));
        assert_eq!(m.current(&now).unwrap().len(), 1);
        m.people().touch("p@x.com", "P", &now).unwrap();
        m.add(&NewClaim::stated("person:p@x.com", "likes golf", "u"), &now)
            .unwrap();
        let r = m.forget_subject("person:p@x.com").unwrap();
        assert_eq!(r.claims_removed, 1);
        assert!(r.person_removed);
        assert!(db.audit_tail(1).unwrap()[0]
            .detail
            .starts_with("subject_claims="));
    }

    #[test]
    fn merge_links_without_deleting_and_expired_lists_stale_claims() {
        let db = setup();
        let m = MemoryStore::new(&db);
        let now = hatch_core::time::now();
        let a = m
            .add(
                &NewClaim::stated("user", "喜欢简短的回复", "user:chat"),
                &now,
            )
            .unwrap()
            .id()
            .to_string();
        let b = m
            .add(
                &NewClaim::stated("user", "喜欢简短的回复。", "user:chat"),
                &now,
            )
            .unwrap()
            .id()
            .to_string();
        m.merge_into(&a, &b, &now).unwrap();
        assert_eq!(m.current(&now).unwrap().len(), 1);
        assert_eq!(
            m.get(&a).unwrap().unwrap().replaced_by.as_deref(),
            Some(b.as_str())
        );
        assert_eq!(
            m.get(&b).unwrap().unwrap().supersedes.as_deref(),
            Some(a.as_str())
        );
        assert!(m.merge_into(&a, &b, &now).is_err(), "already merged");
        let mut stale = NewClaim::stated("user", "本周在上海出差", "user:chat");
        stale.valid_until = Some(now - Duration::days(1));
        m.add(&stale, &(now - Duration::days(3))).unwrap();
        assert_eq!(m.expired(&now).unwrap().len(), 1);
    }

    /// The new persistent-index path must agree with the legacy
    /// full-rebuild path on the same claims (same BM25, same text).
    #[test]
    fn persistent_search_matches_legacy() {
        let db = setup();
        let m = MemoryStore::new(&db);
        let now = hatch_core::time::now();
        let texts = [
            ("preference:reply", "回复邮件要简洁，不要客套话"),
            ("preference:reply", "周五前回复张总的预算邮件"),
            ("preference:meeting", "每周一上午有部门例会"),
            ("preference:meeting", "周三下午不安排会议"),
            ("person:boss@x.com", "老板要求周五前交预算"),
            ("person:boss@x.com", "张总是重要客户，邮件要当天回复"),
            ("user", "Alice prefers morning meetings"),
            ("user", "Bob owns the quarterly budget"),
            ("user", "喜欢喝绿茶，不加糖"),
            ("user", "下周在上海出差，住浦东"),
            ("preference:writing", "写周报用中文，分三段"),
            ("preference:writing", "给客户的方案要附上报价单"),
        ];
        for (subject, text) in texts {
            m.add(&NewClaim::stated(subject, text, "user:chat"), &now)
                .unwrap();
        }
        assert_eq!(m.verify_index().unwrap().indexed_docs, texts.len());
        for q in [
            "回复邮件",
            "例会",
            "预算",
            "绿茶",
            "budget",
            "morning meetings",
            "上海出差",
            "张总",
        ] {
            let new = m.search(q, 5, &now).unwrap();
            let old = m.search_legacy(q, 5, &now).unwrap();
            assert_eq!(new.len(), old.len(), "query {q:?}");
            for ((nc, ns), (oc, os)) in new.iter().zip(old.iter()) {
                assert_eq!(nc.id, oc.id, "query {q:?}");
                assert!((ns - os).abs() < 1e-9, "query {q:?}: score {ns} vs {os}");
            }
        }
    }

    /// A destroyed index must not fail retrieval: rebuild once, and if
    /// that is impossible, fall back to the in-memory build (audited).
    #[test]
    fn corrupt_index_falls_back_to_legacy() {
        let db = setup();
        let m = MemoryStore::new(&db);
        let now = hatch_core::time::now();
        m.add(
            &NewClaim::stated("user", "回复邮件要简洁", "user:chat"),
            &now,
        )
        .unwrap();
        m.add(
            &NewClaim::stated("user", "周三下午不安排会议", "user:chat"),
            &now,
        )
        .unwrap();
        assert_eq!(m.search("会议", 5, &now).unwrap().len(), 1);
        db.conn()
            .execute("DROP TABLE memory_index_terms", [])
            .unwrap();
        let hits = m.search("会议", 5, &now).unwrap();
        assert_eq!(hits.len(), 1, "retrieval still works without the index");
        assert_eq!(hits[0].0.text, "周三下午不安排会议");
        let tail = db.audit_tail(3).unwrap();
        assert!(tail.iter().any(|e| e.event == "index_fallback"), "{tail:?}");
    }

    #[test]
    fn reindex_rebuilds_from_bank() {
        let db = setup();
        let m = MemoryStore::new(&db);
        let now = hatch_core::time::now();
        m.add(
            &NewClaim::stated("user", "回复邮件要简洁", "user:chat"),
            &now,
        )
        .unwrap();
        m.add(
            &NewClaim::stated("user", "周三下午不安排会议", "user:chat"),
            &now,
        )
        .unwrap();
        db.conn()
            .execute("DELETE FROM memory_index_docs", [])
            .unwrap();
        db.conn()
            .execute("DELETE FROM memory_index_terms", [])
            .unwrap();
        let r = m.reindex().unwrap();
        assert!(r.rebuilt && r.indexed_docs == 2, "{r:?}");
        assert_eq!(m.search("会议", 5, &now).unwrap().len(), 1);
        assert!(
            !m.verify_index().unwrap().rebuilt,
            "healthy index is not rebuilt again"
        );
    }

    /// P2: salience defaults to 0.5, `set_salience` hand-tunes it, and
    /// out-of-range values are rejected.
    #[test]
    fn salience_default_and_manual_set() {
        let db = setup();
        let m = MemoryStore::new(&db);
        let now = hatch_core::time::now();
        let id = m
            .add(&NewClaim::stated("user", "喜欢喝绿茶", "user:chat"), &now)
            .unwrap()
            .id()
            .to_string();
        assert_eq!(m.get(&id).unwrap().unwrap().salience, 0.5);
        assert!(m.get(&id).unwrap().unwrap().last_used_at.is_none());
        m.set_salience(&id, 0.9).unwrap();
        assert_eq!(m.get(&id).unwrap().unwrap().salience, 0.9);
        assert!(m.set_salience(&id, 1.5).is_err());
        assert!(m.set_salience("clm_nope", 0.9).is_err());
        let mut bad = NewClaim::stated("user", "x", "user:chat");
        bad.salience = 2.0;
        assert!(m.add(&bad, &now).is_err());
    }

    /// P2: `mark_used` stamps and never moves backward; a plain search
    /// does not mark anything.
    #[test]
    fn mark_used_only_moves_forward_and_search_never_marks() {
        let db = setup();
        let m = MemoryStore::new(&db);
        let now = hatch_core::time::now();
        let id = m
            .add(&NewClaim::stated("user", "周三下午不安排会议", "user:chat"), &now)
            .unwrap()
            .id()
            .to_string();
        assert_eq!(m.search("会议", 5, &now).unwrap().len(), 1);
        assert!(m.get(&id).unwrap().unwrap().last_used_at.is_none(), "search is not use");
        let used = now + Duration::days(1);
        m.mark_used(std::slice::from_ref(&id), &used).unwrap();
        // RFC 3339 storage drops sub-second precision: compare seconds.
        let stamp = |id: &str| m.get(id).unwrap().unwrap().last_used_at.map(|t| t.timestamp());
        assert_eq!(stamp(&id), Some(used.timestamp()));
        m.mark_used(std::slice::from_ref(&id), &now).unwrap();
        assert_eq!(stamp(&id), Some(used.timestamp()), "never moves back");
    }

    /// P2: ranking rules — high salience beats low salience on equal BM25,
    /// long-unused drops below fresh, and pure BM25 order survives when
    /// salience and recency are equal.
    #[test]
    fn salience_and_decay_reorder_search() {
        let db = setup();
        let m = MemoryStore::new(&db);
        let now = hatch_core::time::now();
        let old = now - Duration::days(300);
        let add = |text: &str, salience: f64, at: chrono::DateTime<chrono::Utc>| {
            m.add(
                &NewClaim::stated("preference:reply", text, "user:chat").with_salience(salience),
                &at,
            )
            .unwrap()
            .id()
            .to_string()
        };
        let a = add("回复邮件要简洁", 0.9, now);
        let b = add("回复邮件要热情", 0.2, now);
        let c = add("回复邮件要正式", 0.9, old);
        let d = add("回复邮件回复邮件回复邮件要简洁", 0.5, now);
        let e = add("回复邮件要亲切", 0.5, now);
        let hits = m.search("回复邮件", 10, &now).unwrap();
        let pos = |id: &str| hits.iter().position(|(c, _)| c.id == id).unwrap();
        assert_eq!(hits[0].0.id, a, "high salience wins on equal BM25: {hits:?}");
        assert!(pos(&b) < pos(&c), "fresh low-salience beats stale high-salience");
        assert_eq!(hits.last().unwrap().0.id, c, "300 days unused sinks to the bottom");
        assert!(pos(&d) < pos(&e), "equal salience/recency keeps BM25 order (tf 2 > tf 1)");
    }

    /// P2: the oracle still agrees exactly when salience and last-use
    /// differ — both paths rescore with the same formula.
    #[test]
    fn persistent_search_matches_legacy_with_salience_and_decay() {
        let db = setup();
        let m = MemoryStore::new(&db);
        let now = hatch_core::time::now();
        let a = m
            .add(
                &NewClaim::stated("preference:reply", "回复邮件要简洁，不要客套话", "user:chat")
                    .with_salience(0.9),
                &now,
            )
            .unwrap()
            .id()
            .to_string();
        m.add(
            &NewClaim::stated("preference:reply", "周五前回复张总的预算邮件", "user:chat")
                .with_salience(0.2),
            &now,
        )
        .unwrap();
        m.mark_used(std::slice::from_ref(&a), &(now - Duration::days(200))).unwrap();
        for q in ["回复邮件", "预算", "张总"] {
            let new = m.search(q, 5, &now).unwrap();
            let old = m.search_legacy(q, 5, &now).unwrap();
            assert_eq!(new.len(), old.len(), "query {q:?}");
            for ((nc, ns), (oc, os)) in new.iter().zip(old.iter()) {
                assert_eq!(nc.id, oc.id, "query {q:?}");
                assert!((ns - os).abs() < 1e-9, "query {q:?}: score {ns} vs {os}");
            }
        }
    }

    /// P2 migration: a pre-v9 database (no new columns) gains them with
    /// the documented defaults and keeps its rows.
    #[test]
    fn v9_migration_gives_old_claims_defaults() {
        let db = setup();
        let m = MemoryStore::new(&db);
        let now = hatch_core::time::now();
        let id = m
            .add(&NewClaim::stated("user", "老记忆", "user:chat"), &now)
            .unwrap()
            .id()
            .to_string();
        db.conn().execute("ALTER TABLE claims DROP COLUMN salience", []).unwrap();
        db.conn().execute("ALTER TABLE claims DROP COLUMN last_used_at", []).unwrap();
        db.migrate().unwrap();
        let c = m.get(&id).unwrap().unwrap();
        assert_eq!(c.salience, 0.5, "old claims default to neutral salience");
        assert!(c.last_used_at.is_none(), "old claims were never used");
        assert_eq!(m.search("老记忆", 5, &now).unwrap().len(), 1);
    }
}
