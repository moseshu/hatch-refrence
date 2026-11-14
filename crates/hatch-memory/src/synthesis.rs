//! P3 nightly synthesis: the "current understanding" document.
//!
//! The synthesis worker (`hatch-workers`) summarises the claim bank into
//! one short structured document once a night. The document is a **read-only
//! summary of the claims** — it never becomes a claim itself, never changes
//! salience, and never conflicts with the bank: if a claim changes, the
//! next night's synthesis simply reflects the new bank.
//!
//! Storage is the `meta` table, not a file and not the claims table:
//! * the agent already holds `&Db`, so no home path has to be threaded
//!   through `Agent`/`AgentConfig` to read it;
//! * `set_meta` upserts atomically — a failed run can never leave a
//!   half-written document behind (the worker only saves after a
//!   successful model call and render);
//! * the DB is already the store the daemon, chat and CLI all open.
//!
//! Only the latest document is kept; each synthesis overwrites the
//! previous one. `generated_at` and `source_claims` live in their own
//! keys so readers can show them without parsing the document.

use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::Result;
use hatch_store::Db;

/// Meta keys for the synthesis document and its provenance.
pub const SYNTHESIS_KEY: &str = "memory_synthesis";
pub const SYNTHESIS_GENERATED_AT_KEY: &str = "memory_synthesis:generated_at";
pub const SYNTHESIS_CLAIMS_KEY: &str = "memory_synthesis:claims";
/// Set when a claim was corrected after the document was generated;
/// the value is the correcting claim's id. Cleared on the next save.
pub const SYNTHESIS_STALE_KEY: &str = "memory_synthesis:stale";

/// The stored "current understanding" document.
#[derive(Clone, Debug, PartialEq)]
pub struct SynthesisDoc {
    /// The rendered document (four sections, no header — readers add
    /// their own heading with `generated_at` / `source_claims`).
    pub content: String,
    pub generated_at: Timestamp,
    /// How many claims went into this synthesis.
    pub source_claims: usize,
    /// `Some(correcting claim id)` when a correction landed after this
    /// document was generated; `None` when the document is fresh.
    pub stale: Option<String>,
}

/// Load the latest synthesis, if one was ever generated. A missing or
/// unparsable provenance row means "no synthesis" — never an error, so
/// a damaged row can never fail a turn.
pub fn load_synthesis(db: &Db) -> Result<Option<SynthesisDoc>> {
    let Some(content) = db.meta(SYNTHESIS_KEY)? else {
        return Ok(None);
    };
    let generated_at = db
        .meta(SYNTHESIS_GENERATED_AT_KEY)?
        .and_then(|v| chrono::DateTime::parse_from_rfc3339(&v).ok())
        .map(|d| d.with_timezone(&chrono::Utc))
        .unwrap_or_else(hatch_core::time::now);
    let source_claims = db
        .meta(SYNTHESIS_CLAIMS_KEY)?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let stale = db.meta(SYNTHESIS_STALE_KEY)?.filter(|v| !v.is_empty());
    Ok(Some(SynthesisDoc { content, generated_at, source_claims, stale }))
}

/// Overwrite the stored synthesis with a freshly generated one.
pub fn save_synthesis(db: &Db, doc: &SynthesisDoc) -> Result<()> {
    db.set_meta(SYNTHESIS_KEY, &doc.content)?;
    db.set_meta(SYNTHESIS_GENERATED_AT_KEY, &to_rfc3339(&doc.generated_at))?;
    db.set_meta(SYNTHESIS_CLAIMS_KEY, &doc.source_claims.to_string())?;
    // A fresh document is not stale, whatever a previous correction said.
    db.delete_meta(SYNTHESIS_STALE_KEY)?;
    Ok(())
}

/// Mark the stored synthesis stale because a claim was corrected.
/// No-op when no synthesis was ever generated — there is nothing to
/// go stale. The value names the correcting claim, so the staleness
/// is traceable back to the correction.
pub fn mark_synthesis_stale(db: &Db, correcting_claim_id: &str) -> Result<()> {
    if db.meta(SYNTHESIS_KEY)?.is_none() {
        return Ok(());
    }
    db.set_meta(SYNTHESIS_STALE_KEY, correcting_claim_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NewClaim;
    use hatch_core::time::now;

    #[test]
    fn save_load_roundtrip_and_missing_is_none() {
        let db = Db::open_in_memory().unwrap();
        assert!(load_synthesis(&db).unwrap().is_none());
        let t = now();
        let doc = SynthesisDoc { content: "# 当前理解\n喜欢简洁。".into(), generated_at: t, source_claims: 3, stale: None };
        save_synthesis(&db, &doc).unwrap();
        let back = load_synthesis(&db).unwrap().unwrap();
        assert_eq!(back.content, doc.content);
        assert_eq!(back.source_claims, 3);
        // Overwrite keeps only the latest document.
        let doc2 = SynthesisDoc { content: "新版".into(), generated_at: t, source_claims: 5, stale: None };
        save_synthesis(&db, &doc2).unwrap();
        assert_eq!(load_synthesis(&db).unwrap().unwrap().content, "新版");
        assert_eq!(load_synthesis(&db).unwrap().unwrap().source_claims, 5);
    }

    #[test]
    fn damaged_provenance_degrades_to_defaults_not_an_error() {
        let db = Db::open_in_memory().unwrap();
        db.set_meta(SYNTHESIS_KEY, "doc").unwrap();
        db.set_meta(SYNTHESIS_GENERATED_AT_KEY, "not-a-timestamp").unwrap();
        db.set_meta(SYNTHESIS_CLAIMS_KEY, "not-a-number").unwrap();
        let doc = load_synthesis(&db).unwrap().unwrap();
        assert_eq!(doc.content, "doc");
        assert_eq!(doc.source_claims, 0, "unparsable count falls back to 0");
    }

    #[test]
    fn correction_marks_synthesis_stale_and_resave_clears_it() {
        let db = Db::open_in_memory().unwrap();
        let t = now();
        let mem = crate::MemoryStore::new(&db);
        let old = mem
            .add(&NewClaim::stated("preference:reply", "回复邮件用中文", "user:chat"), &t)
            .unwrap()
            .id()
            .to_string();
        let doc = SynthesisDoc {
            content: "回复邮件用中文".into(),
            generated_at: t,
            source_claims: 1,
            stale: None,
        };
        save_synthesis(&db, &doc).unwrap();
        assert!(load_synthesis(&db).unwrap().unwrap().stale.is_none());
        let new = mem
            .correct(&old, &NewClaim::stated("preference:reply", "回复邮件用英文", "user:cli"), &t)
            .unwrap();
        let back = load_synthesis(&db).unwrap().unwrap();
        assert_eq!(back.stale.as_deref(), Some(new.as_str()));
        // Recall reflects the correction; the old wording is gone.
        let current = mem.current(&t).unwrap();
        assert!(current.iter().any(|c| c.text.contains("回复邮件用英文")));
        assert!(!current.iter().any(|c| c.text.contains("回复邮件用中文")));
        // Explain traces the chain both ways.
        assert_eq!(mem.explain(&new).unwrap().history[0].id, old);
        assert_eq!(mem.explain(&old).unwrap().successors[0].id, new);
        // A fresh synthesis clears the flag.
        let fresh = SynthesisDoc {
            content: "回复邮件用英文".into(),
            generated_at: t,
            source_claims: 1,
            stale: None,
        };
        save_synthesis(&db, &fresh).unwrap();
        assert!(load_synthesis(&db).unwrap().unwrap().stale.is_none());
    }

    #[test]
    fn correction_without_a_synthesis_writes_no_stale_row() {
        let db = Db::open_in_memory().unwrap();
        let t = now();
        let mem = crate::MemoryStore::new(&db);
        let old = mem
            .add(&NewClaim::stated("preference:reply", "回复邮件用中文", "user:chat"), &t)
            .unwrap()
            .id()
            .to_string();
        mem.correct(&old, &NewClaim::stated("preference:reply", "回复邮件用英文", "user:cli"), &t)
            .unwrap();
        assert!(db.meta(SYNTHESIS_STALE_KEY).unwrap().is_none());
    }
}
