//! Claim persistence: add, correct, explain, search, forget.

use crate::claim::{AddOutcome, Claim, Explanation, ForgetReport, NewClaim, FORGET_CAVEAT};
use crate::people::People;
use crate::search::Bm25Index;
use hatch_core::time::{parse_rfc3339, to_rfc3339, Timestamp};
use hatch_core::{ids, secrets, Error, Result, Trust};
use hatch_store::{params, rusqlite, Db, DbResultExt, OptionalExtension};
use std::collections::HashSet;

const MAX_TEXT_CHARS: usize = 2000;
const INFERRED_CAP: f64 = 0.7;
const UNTRUSTED_CAP: f64 = 0.5;

pub struct MemoryStore<'a> {
    db: &'a Db,
}

const COLS: &str =
    "id, subject, text, tags, source, trust, confidence, created_at, valid_until, supersedes, replaced_by, inferred";

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
    })
}

fn normalise(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

impl<'a> MemoryStore<'a> {
    pub fn new(db: &'a Db) -> Self {
        MemoryStore { db }
    }

    pub fn people(&self) -> People<'a> {
        People::new(self.db)
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
            return Err(Error::Invalid(format!("claim text exceeds {MAX_TEXT_CHARS} characters")));
        }
        if c.source.trim().is_empty() {
            return Err(Error::Invalid("claim source is required (where did this come from?)".into()));
        }
        if !(0.0..=1.0).contains(&c.confidence) || c.confidence.is_nan() {
            return Err(Error::Invalid("confidence must be within 0.0..=1.0".into()));
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

    fn insert(&self, c: &NewClaim, now: &Timestamp, supersedes: Option<&str>) -> Result<String> {
        let id = ids::random_id("clm");
        self.db
            .conn()
            .execute(
                &format!("INSERT INTO claims({COLS}) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL, ?11)"),
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
                    c.inferred as i64
                ],
            )
            .db()?;
        Ok(id)
    }

    /// Add a claim (with validation and de-duplication).
    pub fn add(&self, c: &NewClaim, now: &Timestamp) -> Result<AddOutcome> {
        let c = self.validate(c)?;
        let want = normalise(&c.text);
        for existing in self.current_for_subject(&c.subject, now)? {
            if normalise(&existing.text) == want {
                return Ok(AddOutcome::Duplicate(existing.id));
            }
        }
        let id = self.insert(&c, now, None)?;
        self.db.audit("memory", "claim_added", &format!("{id} subject={}", c.subject))?;
        Ok(AddOutcome::Added(id))
    }

    /// Correct a claim: append a new version and link both ways. History is kept.
    pub fn correct(&self, old_id: &str, new: &NewClaim, now: &Timestamp) -> Result<String> {
        let old = self.get(old_id)?.ok_or_else(|| Error::NotFound(format!("claim {old_id}")))?;
        if let Some(r) = &old.replaced_by {
            return Err(Error::Invalid(format!("claim {old_id} was already replaced by {r}; correct the current one")));
        }
        let mut new = self.validate(new)?;
        if new.subject.is_empty() {
            new.subject = old.subject.clone();
        }
        self.db.transaction(|conn| {
            let id = self.insert(&new, now, Some(old_id))?;
            conn.execute("UPDATE claims SET replaced_by = ?2 WHERE id = ?1", params![old_id, id]).db()?;
            Ok(id)
        })
        .and_then(|id| {
            self.db.audit("memory", "claim_corrected", &format!("{old_id} -> {id}"))?;
            Ok(id)
        })
    }

    pub fn get(&self, id: &str) -> Result<Option<Claim>> {
        self.db
            .conn()
            .query_row(&format!("SELECT {COLS} FROM claims WHERE id = ?1"), params![id], row_to_claim)
            .optional()
            .db()
    }

    fn query(&self, sql_where: &str, p: &[&dyn rusqlite::ToSql]) -> Result<Vec<Claim>> {
        let conn = self.db.conn();
        let mut st = conn.prepare(&format!("SELECT {COLS} FROM claims {sql_where}"))
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
            .query("WHERE subject = ?1 AND replaced_by IS NULL ORDER BY created_at", &[&subject])?
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

    /// Ranked search over current claims.
    pub fn search(&self, query: &str, k: usize, now: &Timestamp) -> Result<Vec<(Claim, f64)>> {
        let claims = self.current(now)?;
        let idx = Bm25Index::build(
            claims
                .iter()
                .map(|c| (c.id.as_str(), format!("{} {} {}", c.subject, c.text, c.tags.join(" ")))),
        );
        let hits = idx.query(query, k);
        Ok(hits
            .into_iter()
            .filter_map(|(id, s)| claims.iter().find(|c| c.id == id).map(|c| (c.clone(), s)))
            .collect())
    }

    /// Full provenance: older versions and newer versions. Cycle-safe.
    pub fn explain(&self, id: &str) -> Result<Explanation> {
        let claim = self.get(id)?.ok_or_else(|| Error::NotFound(format!("claim {id}")))?;
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
        Ok(Explanation { claim, history, successors })
    }

    /// Forget a claim and every version in its chain.
    pub fn forget_claim(&self, id: &str) -> Result<ForgetReport> {
        let ex = self.explain(id)?;
        let mut all: Vec<String> = vec![ex.claim.id.clone()];
        all.extend(ex.history.iter().map(|c| c.id.clone()));
        all.extend(ex.successors.iter().map(|c| c.id.clone()));
        self.db.transaction(|conn| {
            for cid in &all {
                conn.execute("DELETE FROM claims WHERE id = ?1", params![cid]).db()?;
            }
            Ok(())
        })?;
        self.db.audit("memory", "forget", &format!("claims={}", all.join(",")))?;
        Ok(ForgetReport { claims_removed: all.len(), person_removed: false, caveat: FORGET_CAVEAT.into() })
    }

    /// Forget everything about a subject (all versions), and the person page if it is `person:<email>`.
    pub fn forget_subject(&self, subject: &str) -> Result<ForgetReport> {
        let ids: Vec<String> = self.query("WHERE subject = ?1", &[&subject])?.into_iter().map(|c| c.id).collect();
        self.db.transaction(|conn| {
            conn.execute("DELETE FROM claims WHERE subject = ?1", params![subject]).db()?;
            Ok(())
        })?;
        let person_removed = match subject.strip_prefix("person:") {
            Some(email) => self.people().remove(email)?,
            None => false,
        };
        self.db.audit("memory", "forget", &format!("subject_claims={} person={person_removed}", ids.join(",")))?;
        Ok(ForgetReport { claims_removed: ids.len(), person_removed, caveat: FORGET_CAVEAT.into() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

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
        let b = m.add(&NewClaim::stated("preference:reply", " 回复要简短，不要客套 ", "user:chat"), &now).unwrap();
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
        let e = m.add(&NewClaim::stated("account", "126 邮箱授权码：ABCD1234XYZ", "user:chat"), &now).unwrap_err();
        assert!(matches!(e, Error::SecretRejected(_)));
        assert!(!e.to_string().contains("ABCD1234XYZ"));
        let e = m.add(&NewClaim::stated("bank", "卡号 4111111111111111", "user:chat"), &now).unwrap_err();
        assert!(matches!(e, Error::SecretRejected(_)));
        assert_eq!(m.current(&now).unwrap().len(), 0);
    }

    #[test]
    fn confidence_caps() {
        let db = setup();
        let m = MemoryStore::new(&db);
        let now = hatch_core::time::now();
        let id = m.add(&NewClaim::stated("u", "likes tea", "mail:1").with_trust(Trust::Untrusted, 0.95), &now).unwrap();
        assert_eq!(m.get(id.id()).unwrap().unwrap().confidence, 0.5);
        let id = m.add(&NewClaim::stated("u", "works late", "behaviour").inferred(), &now).unwrap();
        assert_eq!(m.get(id.id()).unwrap().unwrap().confidence, 0.7);
    }

    #[test]
    fn correction_chain_and_explain() {
        let db = setup();
        let m = MemoryStore::new(&db);
        let now = hatch_core::time::now();
        let a = m.add(&NewClaim::stated("person:li@x.com", "李雷在 A 公司", "user:chat"), &now).unwrap();
        let b = m.correct(a.id(), &NewClaim::stated("person:li@x.com", "李雷跳槽到 B 公司", "user:chat"), &now).unwrap();
        let c = m.correct(&b, &NewClaim::stated("person:li@x.com", "李雷在 B 公司任总监", "user:chat"), &now).unwrap();
        assert!(m.correct(a.id(), &NewClaim::stated("s", "t", "u"), &now).is_err());
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
        assert_eq!(m.search("出差", 3, &(now + Duration::days(2))).unwrap().len(), 0);
    }

    #[test]
    fn forget_removes_whole_chain() {
        let db = setup();
        let m = MemoryStore::new(&db);
        let now = hatch_core::time::now();
        let a = m.add(&NewClaim::stated("s", "v1", "u"), &now).unwrap();
        let b = m.correct(a.id(), &NewClaim::stated("s", "v2", "u"), &now).unwrap();
        m.add(&NewClaim::stated("other", "keep", "u"), &now).unwrap();
        let r = m.forget_claim(&b).unwrap();
        assert_eq!(r.claims_removed, 2);
        assert!(r.caveat.contains("无法被撤回"));
        assert_eq!(m.current(&now).unwrap().len(), 1);
        m.people().touch("p@x.com", "P", &now).unwrap();
        m.add(&NewClaim::stated("person:p@x.com", "likes golf", "u"), &now).unwrap();
        let r = m.forget_subject("person:p@x.com").unwrap();
        assert_eq!(r.claims_removed, 1);
        assert!(r.person_removed);
        assert!(db.audit_tail(1).unwrap()[0].detail.starts_with("subject_claims="));
    }
}
