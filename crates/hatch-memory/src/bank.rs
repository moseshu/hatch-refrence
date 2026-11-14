//! The bank — claim persistence (add, correct, explain, forget, merge).
//!
//! The bank owns the facts in `claims`. Every mutation updates the
//! persistent index (`index.rs`) in the *same* transaction, so bank and
//! index cannot drift apart on a crash. Reads that need ranking go
//! through the store, which consults the index.

use crate::claim::{AddOutcome, Claim, Explanation, ForgetReport, NewClaim, FORGET_CAVEAT};
use crate::index::{doc_text, index_claim_on, remove_claim_on};
use hatch_core::time::{parse_rfc3339, to_rfc3339, Timestamp};
use hatch_core::{ids, secrets, Error, Result, Trust};
use hatch_store::{params, rusqlite, Connection, Db, DbResultExt, OptionalExtension};
use std::collections::HashSet;

const MAX_TEXT_CHARS: usize = 2000;
const INFERRED_CAP: f64 = 0.7;
const UNTRUSTED_CAP: f64 = 0.5;

pub struct Bank<'a> {
    db: &'a Db,
}

const COLS: &str =
    "id, subject, text, tags, source, trust, confidence, created_at, valid_until, supersedes, replaced_by, inferred, salience, last_used_at";

fn row_to_claim(r: &rusqlite::Row) -> rusqlite::Result<Claim> {
    let tags: String = r.get(3)?;
    let trust: String = r.get(5)?;
    let created: String = r.get(7)?;
    let valid: Option<String> = r.get(8)?;
    Ok(Claim {
        id: r.get(0)?,
        subject: r.get(1)?,
        text: r.get(2)?,
        tags: serde_json::from_str(&tags).unwrap_or_default(),
        source: r.get(4)?,
        trust: Trust::parse(&trust).unwrap_or(Trust::Untrusted),
        confidence: r.get(6)?,
        created_at: parse_rfc3339(&created).unwrap_or_default(),
        valid_until: valid.and_then(|v| parse_rfc3339(&v).ok()),
        supersedes: r.get(9)?,
        replaced_by: r.get(10)?,
        inferred: r.get::<_, i64>(11)? != 0,
        salience: r.get(12)?,
        last_used_at: r.get::<_, Option<String>>(13)?.and_then(|v| parse_rfc3339(&v).ok()),
    })
}

fn normalise(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

impl<'a> Bank<'a> {
    pub fn new(db: &'a Db) -> Self {
        Bank { db }
    }

    /// Enforce write discipline and return the (possibly capped) claim input.
    fn validate(&self, c: &NewClaim) -> Result<NewClaim> {
        let mut c = c.clone();
        c.text = c.text.trim().to_string();
        c.subject = c.subject.trim().to_string();
        if c.text.is_empty() || c.subject.is_empty() {
            return Err(Error::Invalid("claim subject and text are required".into()));
        }
        if c.text.chars().count() > MAX_TEXT_CHARS {
            return Err(Error::Invalid(format!(
                "claim text exceeds {MAX_TEXT_CHARS} characters"
            )));
        }
        if c.source.trim().is_empty() {
            return Err(Error::Invalid(
                "claim source is required (where did this come from?)".into(),
            ));
        }
        if !(0.0..=1.0).contains(&c.confidence) || c.confidence.is_nan() {
            return Err(Error::Invalid("confidence must be within 0.0..=1.0".into()));
        }
        if !(0.0..=1.0).contains(&c.salience) || c.salience.is_nan() {
            return Err(Error::Invalid("salience must be within 0.0..=1.0".into()));
        }
        let found = secrets::kinds(&format!("{} {} {}", c.subject, c.text, c.tags.join(" ")));
        if !found.is_empty() {
            let k: Vec<&str> = found.iter().map(|k| k.as_str()).collect();
            return Err(Error::SecretRejected(format!(
                "memory never stores credentials or sensitive numbers (detected: {})",
                k.join(", ")
            )));
        }
        if c.inferred {
            c.confidence = c.confidence.min(INFERRED_CAP);
        }
        if c.trust.is_untrusted() {
            c.confidence = c.confidence.min(UNTRUSTED_CAP);
        }
        Ok(c)
    }

    fn insert_on(
        conn: &Connection,
        c: &NewClaim,
        now: &Timestamp,
        supersedes: Option<&str>,
    ) -> Result<String> {
        let id = ids::random_id("clm");
        conn.execute(
            &format!("INSERT INTO claims({COLS}) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL, ?11, ?12, NULL)"),
            params![
                id,
                c.subject,
                c.text,
                serde_json::to_string(&c.tags)?,
                c.source,
                c.trust.as_str(),
                c.confidence,
                to_rfc3339(now),
                c.valid_until.map(|v| to_rfc3339(&v)),
                supersedes,
                c.inferred as i64, c.salience
            ],
        )
        .db()?;
        Ok(id)
    }

    /// P2: stamp claims injected into a turn's context. Best-effort; a
    /// plain search never calls this — searching is not using.
    pub fn mark_used(&self, ids: &[String], now: &Timestamp) -> Result<()> {
        let at = to_rfc3339(now);
        self.db.transaction(|conn| {
            for id in ids {
                conn.execute("UPDATE claims SET last_used_at = MAX(COALESCE(last_used_at, ''), ?2) WHERE id = ?1", params![id, at]).db()?;
            }
            Ok(())
        })
    }

    /// P2: hand-tune a claim's ranking weight. Metadata only — no new version.
    pub fn set_salience(&self, id: &str, salience: f64) -> Result<()> {
        if !(0.0..=1.0).contains(&salience) || salience.is_nan() {
            return Err(Error::Invalid("salience must be within 0.0..=1.0".into()));
        }
        self.get(id)?.ok_or_else(|| Error::NotFound(format!("claim {id}")))?;
        self.db.conn().execute("UPDATE claims SET salience = ?2 WHERE id = ?1", params![id, salience]).db()?;
        self.db.audit("memory", "salience_set", &format!("{id} salience={salience:.2}"))
    }

    /// Add a claim (with validation and de-duplication). The index entry
    /// is written in the same transaction as the claim.
    pub fn add(&self, c: &NewClaim, now: &Timestamp) -> Result<AddOutcome> {
        let c = self.validate(c)?;
        let want = normalise(&c.text);
        for existing in self.current_for_subject(&c.subject, now)? {
            if normalise(&existing.text) == want {
                return Ok(AddOutcome::Duplicate(existing.id));
            }
        }
        let id = self.db.transaction(|conn| {
            let id = Self::insert_on(conn, &c, now, None)?;
            index_claim_on(conn, &id, &doc_text(&c.subject, &c.text, &c.tags))?;
            Ok(id)
        })?;
        self.db.audit(
            "memory",
            "claim_added",
            &format!("{id} subject={}", c.subject),
        )?;
        Ok(AddOutcome::Added(id))
    }

    /// Correct a claim: append a new version and link both ways. History is kept.
    /// The old claim leaves the index; the new version enters it.
    pub fn correct(&self, old_id: &str, new: &NewClaim, now: &Timestamp) -> Result<String> {
        let old = self
            .get(old_id)?
            .ok_or_else(|| Error::NotFound(format!("claim {old_id}")))?;
        if let Some(r) = &old.replaced_by {
            return Err(Error::Invalid(format!(
                "claim {old_id} was already replaced by {r}; correct the current one"
            )));
        }
        let mut new = self.validate(new)?;
        if new.subject.is_empty() {
            new.subject = old.subject.clone();
        }
        self.db
            .transaction(|conn| {
                let id = Self::insert_on(conn, &new, now, Some(old_id))?;
                conn.execute(
                    "UPDATE claims SET replaced_by = ?2 WHERE id = ?1",
                    params![old_id, id],
                )
                .db()?;
                remove_claim_on(conn, old_id)?;
                index_claim_on(conn, &id, &doc_text(&new.subject, &new.text, &new.tags))?;
                Ok(id)
            })
            .and_then(|id| {
                self.db
                    .audit("memory", "claim_corrected", &format!("{old_id} -> {id}"))?;
                // The stored "current understanding" was rendered from
                // the old claim: mark it stale so readers know it may
                // be out of date until the next synthesis.
                crate::synthesis::mark_synthesis_stale(self.db, &id)?;
                Ok(id)
            })
    }

    pub fn get(&self, id: &str) -> Result<Option<Claim>> {
        self.db
            .conn()
            .query_row(
                &format!("SELECT {COLS} FROM claims WHERE id = ?1"),
                params![id],
                row_to_claim,
            )
            .optional()
            .db()
    }

    fn query(&self, sql_where: &str, p: &[&dyn rusqlite::ToSql]) -> Result<Vec<Claim>> {
        let conn = self.db.conn();
        let mut st = conn
            .prepare(&format!("SELECT {COLS} FROM claims {sql_where}"))
            .db()?;
        let rows = st.query_map(p, row_to_claim).db()?;
        rows.collect::<rusqlite::Result<Vec<_>>>().db()
    }

    /// All current claims.
    pub fn current(&self, now: &Timestamp) -> Result<Vec<Claim>> {
        Ok(self
            .query("WHERE replaced_by IS NULL ORDER BY created_at", &[])?
            .into_iter()
            .filter(|c| c.is_current(now))
            .collect())
    }

    pub fn current_for_subject(&self, subject: &str, now: &Timestamp) -> Result<Vec<Claim>> {
        Ok(self
            .query(
                "WHERE subject = ?1 AND replaced_by IS NULL ORDER BY created_at",
                &[&subject],
            )?
            .into_iter()
            .filter(|c| c.is_current(now))
            .collect())
    }

    /// Current claims carrying any of `tags` (e.g. `preference`, `style`).
    pub fn with_tags(&self, tags: &[&str], now: &Timestamp) -> Result<Vec<Claim>> {
        Ok(self
            .current(now)?
            .into_iter()
            .filter(|c| c.tags.iter().any(|t| tags.contains(&t.as_str())))
            .collect())
    }

    /// Full provenance: older versions and newer versions. Cycle-safe.
    pub fn explain(&self, id: &str) -> Result<Explanation> {
        let claim = self
            .get(id)?
            .ok_or_else(|| Error::NotFound(format!("claim {id}")))?;
        let mut seen: HashSet<String> = HashSet::from([claim.id.clone()]);
        let mut history = Vec::new();
        let mut cur = claim.supersedes.clone();
        while let Some(prev) = cur {
            if !seen.insert(prev.clone()) {
                break;
            }
            match self.get(&prev)? {
                Some(c) => {
                    cur = c.supersedes.clone();
                    history.push(c);
                }
                None => break,
            }
        }
        let mut successors = Vec::new();
        let mut cur = claim.replaced_by.clone();
        while let Some(next) = cur {
            if !seen.insert(next.clone()) {
                break;
            }
            match self.get(&next)? {
                Some(c) => {
                    cur = c.replaced_by.clone();
                    successors.push(c);
                }
                None => break,
            }
        }
        Ok(Explanation {
            claim,
            history,
            successors,
        })
    }

    /// Forget a claim and every version in its chain.
    pub fn forget_claim(&self, id: &str) -> Result<ForgetReport> {
        let ex = self.explain(id)?;
        let mut all: Vec<String> = vec![ex.claim.id.clone()];
        all.extend(ex.history.iter().map(|c| c.id.clone()));
        all.extend(ex.successors.iter().map(|c| c.id.clone()));
        self.db.transaction(|conn| {
            for cid in &all {
                conn.execute("DELETE FROM claims WHERE id = ?1", params![cid])
                    .db()?;
                remove_claim_on(conn, cid)?;
            }
            Ok(())
        })?;
        self.db
            .audit("memory", "forget", &format!("claims={}", all.join(",")))?;
        Ok(ForgetReport {
            claims_removed: all.len(),
            person_removed: false,
            caveat: FORGET_CAVEAT.into(),
        })
    }

    /// Forget everything about a subject (all versions), and the person page if it is `person:<email>`.
    /// The person-page removal is the store's job; this only handles claims.
    pub fn forget_subject_claims(&self, subject: &str) -> Result<Vec<String>> {
        let ids: Vec<String> = self
            .query("WHERE subject = ?1", &[&subject])?
            .into_iter()
            .map(|c| c.id)
            .collect();
        self.db.transaction(|conn| {
            conn.execute("DELETE FROM claims WHERE subject = ?1", params![subject])
                .db()?;
            for cid in &ids {
                remove_claim_on(conn, cid)?;
            }
            Ok(())
        })?;
        Ok(ids)
    }

    /// Merge a near-duplicate: mark `old_id` as replaced by the existing
    /// current claim `keep_id`. Nothing is deleted — `explain` shows the link.
    /// The merged-away claim leaves the index.
    pub fn merge_into(&self, old_id: &str, keep_id: &str, now: &Timestamp) -> Result<()> {
        let old = self
            .get(old_id)?
            .ok_or_else(|| Error::NotFound(format!("claim {old_id}")))?;
        let keep = self
            .get(keep_id)?
            .ok_or_else(|| Error::NotFound(format!("claim {keep_id}")))?;
        if old_id == keep_id || old.subject != keep.subject {
            return Err(Error::Invalid(
                "merge needs two different claims about the same subject".into(),
            ));
        }
        if old.replaced_by.is_some() || !keep.is_current(now) {
            return Err(Error::Invalid(format!(
                "{old_id} is already replaced or {keep_id} is not current"
            )));
        }
        self.db.transaction(|conn| {
            conn.execute(
                "UPDATE claims SET replaced_by = ?2 WHERE id = ?1",
                params![old_id, keep_id],
            )
            .db()?;
            conn.execute(
                "UPDATE claims SET supersedes = COALESCE(supersedes, ?2) WHERE id = ?1",
                params![keep_id, old_id],
            )
            .db()?;
            remove_claim_on(conn, old_id)?;
            Ok(())
        })?;
        self.db.audit(
            "memory",
            "claim_merged",
            &format!("{old_id} -> {keep_id} (near-duplicate)"),
        )
    }

    /// Claims whose `valid_until` has passed and that were never replaced.
    pub fn expired(&self, now: &Timestamp) -> Result<Vec<Claim>> {
        Ok(self
            .query(
                "WHERE replaced_by IS NULL AND valid_until IS NOT NULL ORDER BY valid_until",
                &[],
            )?
            .into_iter()
            .filter(|c| !c.is_current(now))
            .collect())
    }
}
