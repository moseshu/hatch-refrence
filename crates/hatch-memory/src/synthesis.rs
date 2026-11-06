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

/// The stored "current understanding" document.
#[derive(Clone, Debug, PartialEq)]
pub struct SynthesisDoc {
    /// The rendered document (four sections, no header — readers add
    /// their own heading with `generated_at` / `source_claims`).
    pub content: String,
    pub generated_at: Timestamp,
    /// How many claims went into this synthesis.
    pub source_claims: usize,
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
    Ok(Some(SynthesisDoc { content, generated_at, source_claims }))
}

/// Overwrite the stored synthesis with a freshly generated one.
pub fn save_synthesis(db: &Db, doc: &SynthesisDoc) -> Result<()> {
    db.set_meta(SYNTHESIS_KEY, &doc.content)?;
    db.set_meta(SYNTHESIS_GENERATED_AT_KEY, &to_rfc3339(&doc.generated_at))?;
    db.set_meta(SYNTHESIS_CLAIMS_KEY, &doc.source_claims.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatch_core::time::now;

    #[test]
    fn save_load_roundtrip_and_missing_is_none() {
        let db = Db::open_in_memory().unwrap();
        assert!(load_synthesis(&db).unwrap().is_none());
        let t = now();
        let doc = SynthesisDoc { content: "# 当前理解\n喜欢简洁。".into(), generated_at: t, source_claims: 3 };
        save_synthesis(&db, &doc).unwrap();
        let back = load_synthesis(&db).unwrap().unwrap();
        assert_eq!(back.content, doc.content);
        assert_eq!(back.source_claims, 3);
        // Overwrite keeps only the latest document.
        let doc2 = SynthesisDoc { content: "新版".into(), generated_at: t, source_claims: 5 };
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
}
