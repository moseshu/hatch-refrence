//! The persistent BM25 index — the "index" half of the bank/index split.
//!
//! The bank (`claims`) owns the facts; this module owns the retrieval
//! structure: an inverted index (`memory_index_terms`: term -> (claim_id, tf))
//! plus per-doc lengths (`memory_index_docs`), in `memory_index_*` tables.
//! Scoring is the same BM25 (k1 = 1.2, b = 0.75, CJK unigram weight 0.3)
//! as the in-memory index in `search.rs`, so results agree with the legacy
//! full-rebuild path.
//!
//! Bank writes update the index in the same transaction, so the two cannot
//! drift apart on a crash. `verify()` is the self-healing step: a dirty flag
//! or a doc-count mismatch triggers a full rebuild. Expired claims stay
//! indexed (expiry is time-dependent) and are filtered at query time by the
//! store.

use crate::search::tokenize;
use hatch_core::tokens::is_cjk;
use hatch_core::Result;
use hatch_store::{params, rusqlite, Connection, Db, DbResultExt, OptionalExtension};
use serde::Serialize;
use std::cmp::Ordering;
use std::collections::HashMap;

const K1: f64 = 1.2;
const B: f64 = 0.75;
/// Candidates returned to the store, which filters expired/replaced rows
/// and truncates to k. 10x the largest k any caller uses (20).
const MAX_CANDIDATES: usize = 200;
const META_DIRTY: &str = "dirty";

/// Outcome of an index check (and of a rebuild, when one ran).
#[derive(Clone, Debug, Default, Serialize)]
pub struct IndexReport {
    /// Claims currently in the bank (not replaced).
    pub bank_docs: usize,
    /// Docs present in the index after the check.
    pub indexed_docs: usize,
    /// Whether a full rebuild ran.
    pub rebuilt: bool,
    /// Why a rebuild ran, or "ok" when the index was already healthy.
    pub reason: String,
}

/// Text the index is built over: subject + text + tags, same as the
/// legacy in-memory search.
pub fn doc_text(subject: &str, text: &str, tags: &[String]) -> String {
    format!("{} {} {}", subject, text, tags.join(" "))
}

pub struct MemoryIndex<'a> {
    db: &'a Db,
}

/// Index one claim's text on `conn`. Idempotent: previous rows for the
/// claim are removed first. Call inside the bank write's transaction.
pub(crate) fn index_claim_on(conn: &Connection, claim_id: &str, text: &str) -> Result<()> {
    remove_claim_on(conn, claim_id)?;
    let toks = tokenize(text);
    let len = toks.len() as i64;
    let mut tf: HashMap<String, i64> = HashMap::new();
    for t in toks {
        *tf.entry(t).or_default() += 1;
    }
    conn.execute(
        "INSERT INTO memory_index_docs(claim_id, len) VALUES(?1, ?2)",
        params![claim_id, len],
    )
    .db()?;
    for (term, count) in tf {
        conn.execute(
            "INSERT INTO memory_index_terms(term, claim_id, tf) VALUES(?1, ?2, ?3)",
            params![term, claim_id, count],
        )
        .db()?;
    }
    Ok(())
}

/// Drop one claim from the index. No-op when it was never indexed.
pub(crate) fn remove_claim_on(conn: &Connection, claim_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM memory_index_terms WHERE claim_id = ?1",
        params![claim_id],
    )
    .db()?;
    conn.execute(
        "DELETE FROM memory_index_docs WHERE claim_id = ?1",
        params![claim_id],
    )
    .db()?;
    Ok(())
}

fn set_dirty(conn: &Connection, dirty: bool) -> Result<()> {
    conn.execute(
        "INSERT INTO memory_index_meta(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![META_DIRTY, if dirty { "1" } else { "0" }],
    )
    .db()?;
    Ok(())
}

impl<'a> MemoryIndex<'a> {
    pub fn new(db: &'a Db) -> Self {
        MemoryIndex { db }
    }

    fn is_dirty(&self) -> Result<bool> {
        let v: Option<String> = self
            .db
            .conn()
            .query_row(
                "SELECT value FROM memory_index_meta WHERE key = ?1",
                params![META_DIRTY],
                |r| r.get(0),
            )
            .optional()
            .db()?;
        Ok(v.as_deref() == Some("1"))
    }

    fn bank_doc_count(&self) -> Result<usize> {
        let n: i64 = self
            .db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM claims WHERE replaced_by IS NULL",
                [],
                |r| r.get(0),
            )
            .db()?;
        Ok(n as usize)
    }

    fn indexed_doc_count(&self) -> Result<usize> {
        let n: i64 = self
            .db
            .conn()
            .query_row("SELECT COUNT(*) FROM memory_index_docs", [], |r| r.get(0))
            .db()?;
        Ok(n as usize)
    }

    /// Full rebuild: drop every index row and re-index all non-replaced
    /// claims. One transaction, so a crash mid-rebuild leaves the dirty
    /// flag set and the next `verify()` retries.
    pub fn rebuild(&self) -> Result<IndexReport> {
        let bank_docs = self.bank_doc_count()?;
        self.db.transaction(|conn| {
            conn.execute_batch("DELETE FROM memory_index_terms; DELETE FROM memory_index_docs;").db()?;
            set_dirty(conn, true)?;
            let mut st = conn
                .prepare("SELECT id, subject, text, tags FROM claims WHERE replaced_by IS NULL ORDER BY created_at")
                .db()?;
            let rows = st
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                })
                .db()?;
            let rows: Vec<(String, String, String, String)> = rows.collect::<rusqlite::Result<_>>().db()?;
            for (id, subject, text, tags) in &rows {
                let tags: Vec<String> = serde_json::from_str(tags).unwrap_or_default();
                index_claim_on(conn, id, &doc_text(subject, text, &tags))?;
            }
            set_dirty(conn, false)?;
            Ok(())
        })?;
        let indexed_docs = self.indexed_doc_count()?;
        Ok(IndexReport {
            bank_docs,
            indexed_docs,
            rebuilt: true,
            reason: "rebuilt".into(),
        })
    }

    /// Self-healing check: a dirty flag or a doc-count mismatch rebuilds
    /// the index. Cheap when healthy (two COUNT queries).
    pub fn verify(&self) -> Result<IndexReport> {
        let dirty = self.is_dirty()?;
        let bank_docs = self.bank_doc_count()?;
        let indexed_docs = self.indexed_doc_count()?;
        if !dirty && bank_docs == indexed_docs {
            return Ok(IndexReport {
                bank_docs,
                indexed_docs,
                rebuilt: false,
                reason: "ok".into(),
            });
        }
        let reason = if dirty {
            "dirty flag set".to_string()
        } else {
            format!("doc count mismatch: bank={bank_docs} index={indexed_docs}")
        };
        let mut report = self.rebuild()?;
        report.reason = reason;
        Ok(report)
    }

    /// Ranked claim ids for `q`, best first, score > 0. Self-heals first.
    /// Returns up to MAX_CANDIDATES ids; the store filters expired or
    /// replaced rows and truncates to k.
    pub fn query(&self, q: &str) -> Result<Vec<(String, f64)>> {
        self.verify()?;
        let mut qt = tokenize(q);
        qt.sort();
        qt.dedup();
        if qt.is_empty() {
            return Ok(vec![]);
        }
        let conn = self.db.conn();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM memory_index_docs", [], |r| r.get(0))
            .db()?;
        if n == 0 {
            return Ok(vec![]);
        }
        let n = n as f64;
        let total_len: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(len), 0) FROM memory_index_docs",
                [],
                |r| r.get(0),
            )
            .db()?;
        let avgdl = (total_len as f64 / n).max(1.0);
        let mut scores: HashMap<String, f64> = HashMap::new();
        for t in &qt {
            let mut st = conn
                .prepare(
                    "SELECT t.claim_id, t.tf, d.len FROM memory_index_terms t
                     JOIN memory_index_docs d ON d.claim_id = t.claim_id
                     WHERE t.term = ?1",
                )
                .db()?;
            let postings = st
                .query_map(params![t], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })
                .db()?;
            let postings: Vec<(String, i64, i64)> =
                postings.collect::<rusqlite::Result<_>>().db()?;
            let df = postings.len() as f64;
            if df == 0.0 {
                continue;
            }
            let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
            let weight = if t.chars().count() == 1 && t.chars().next().map(is_cjk).unwrap_or(false)
            {
                0.3
            } else {
                1.0
            };
            for (id, tf, len) in postings {
                let f = tf as f64;
                let len = len as f64;
                *scores.entry(id).or_default() +=
                    weight * idf * f * (K1 + 1.0) / (f + K1 * (1.0 - B + B * len / avgdl));
            }
        }
        let mut out: Vec<(String, f64)> = scores.into_iter().filter(|(_, s)| *s > 0.0).collect();
        out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));
        out.truncate(MAX_CANDIDATES);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claim::NewClaim;
    use crate::store::MemoryStore;
    use hatch_core::time::now;

    fn seeded() -> (Db, Vec<String>) {
        let db = Db::open_in_memory().unwrap();
        let m = MemoryStore::new(&db);
        let t = now();
        let ids = [
            "回复邮件要简洁，不要客套",
            "周三下午不安排会议",
            "张总是重要客户",
        ]
        .iter()
        .map(|text| {
            m.add(&NewClaim::stated("user", text, "user:chat"), &t)
                .unwrap()
                .id()
                .to_string()
        })
        .collect();
        (db, ids)
    }

    #[test]
    fn incremental_update_and_verify() {
        let (db, ids) = seeded();
        let idx = MemoryIndex::new(&db);
        let r = idx.verify().unwrap();
        assert!(!r.rebuilt && r.indexed_docs == 3, "{r:?}");
        // A corrected claim leaves the index: old id unfindable, new id found.
        let m = MemoryStore::new(&db);
        let t = now();
        let new = m
            .correct(
                &ids[0],
                &NewClaim::stated("user", "回复邮件可以用表情", "user:chat"),
                &t,
            )
            .unwrap();
        let hits = idx.query("简洁").unwrap();
        assert!(
            hits.iter().all(|(id, _)| id != &ids[0]),
            "replaced claim must not hit"
        );
        let hits = idx.query("表情").unwrap();
        assert_eq!(hits[0].0, new);
        let r = idx.verify().unwrap();
        assert!(!r.rebuilt && r.indexed_docs == 3, "{r:?}");
    }

    #[test]
    fn remove_on_forget_and_merge() {
        let (db, ids) = seeded();
        let m = MemoryStore::new(&db);
        let idx = MemoryIndex::new(&db);
        let t = now();
        m.forget_claim(&ids[1]).unwrap();
        assert!(idx.query("会议").unwrap().is_empty());
        let a = m
            .add(&NewClaim::stated("user", "喜欢喝绿茶", "user:chat"), &t)
            .unwrap()
            .id()
            .to_string();
        let b = m
            .add(&NewClaim::stated("user", "喜欢喝绿茶。", "user:chat"), &t)
            .unwrap()
            .id()
            .to_string();
        m.merge_into(&a, &b, &t).unwrap();
        let hits = idx.query("绿茶").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, b);
    }

    #[test]
    fn rebuild_repairs_deleted_rows() {
        let (db, ids) = seeded();
        let idx = MemoryIndex::new(&db);
        // Simulate corruption: drop half the index behind the bank's back.
        db.conn()
            .execute(
                "DELETE FROM memory_index_terms WHERE claim_id = ?1",
                params![ids[0]],
            )
            .unwrap();
        db.conn()
            .execute(
                "DELETE FROM memory_index_docs WHERE claim_id = ?1",
                params![ids[0]],
            )
            .unwrap();
        let r = idx.verify().unwrap();
        assert!(r.rebuilt, "{r:?}");
        assert!(r.reason.contains("mismatch"));
        assert_eq!(r.indexed_docs, 3);
        assert_eq!(idx.query("简洁").unwrap()[0].0, ids[0]);
        // A stale dirty flag also triggers a rebuild.
        db.conn()
            .execute(
                "INSERT INTO memory_index_meta(key, value) VALUES('dirty', '1')
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [],
            )
            .unwrap();
        assert!(idx.verify().unwrap().rebuilt);
    }

    #[test]
    fn empty_index_queries_empty() {
        let db = Db::open_in_memory().unwrap();
        let idx = MemoryIndex::new(&db);
        assert!(idx.query("anything").unwrap().is_empty());
        assert!(!idx.verify().unwrap().rebuilt);
    }
}
