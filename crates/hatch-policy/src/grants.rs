//! Standing grants ("常设授权").
//!
//! A grant pre-approves a class of action for a set of recipients, optionally
//! limited by use count and expiry. By default a grant does **not** apply when
//! the triggering context contains untrusted content (e.g. a reply drafted from
//! an incoming mail) — the user must opt into that explicitly.

use crate::action::ActionClass;
use hatch_core::time::{parse_rfc3339, to_rfc3339, Timestamp};
use hatch_core::{ids, Error, Result};
use hatch_store::{params, rusqlite, Db, DbResultExt};
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct Grant {
    pub id: String,
    pub class: ActionClass,
    /// `*`, `*@corp.com` or exact addresses.
    pub recipients: Vec<String>,
    pub max_uses: Option<i64>,
    pub used: i64,
    pub allow_when_tainted: bool,
    pub expires_at: Option<Timestamp>,
    pub created_at: Timestamp,
    pub revoked: bool,
    pub note: String,
}

#[derive(Clone, Debug)]
pub struct NewGrant {
    pub class: ActionClass,
    pub recipients: Vec<String>,
    pub max_uses: Option<i64>,
    pub allow_when_tainted: bool,
    pub expires_at: Option<Timestamp>,
    pub note: String,
}

/// Does `addr` match a recipient pattern?
pub fn pattern_matches(pattern: &str, addr: &str) -> bool {
    let p = pattern.trim().to_ascii_lowercase();
    let a = addr.trim().to_ascii_lowercase();
    if p == "*" {
        return true;
    }
    match p.strip_prefix("*@") {
        Some(domain) => a.rsplit_once('@').map(|(_, d)| d == domain).unwrap_or(false),
        None => p == a,
    }
}

/// Shell metacharacters that would let a command escape a prefix grant.
const SHELL_META: &[char] = &[';', '&', '|', '$', '`', '<', '>', '(', ')', '{', '}', '!', '\n', '\\', '*', '?', '~'];

/// `cargo test` (exact) or `cargo test*` (prefix, but the rest must be plain arguments).
pub fn command_pattern_matches(pattern: &str, command: &str) -> bool {
    let (p, c) = (pattern.trim(), command.trim());
    if p == c {
        return true;
    }
    match p.strip_suffix('*') {
        Some(prefix) => c.starts_with(prefix) && !c[prefix.len()..].contains(SHELL_META) && !prefix.contains(SHELL_META),
        None => false,
    }
}

/// `*`, an exact workspace-relative path, or `dir/*` / `dir/**` for a subtree.
pub fn path_pattern_matches(pattern: &str, path: &str) -> bool {
    let (p, a) = (pattern.trim().trim_start_matches("./"), path.trim().trim_start_matches("./"));
    if a.split('/').any(|seg| seg == "..") {
        return false;
    }
    if p == "*" || p == a {
        return true;
    }
    let dir = p.strip_suffix("/**").or_else(|| p.strip_suffix("/*"));
    match dir {
        Some(d) if p.ends_with("/**") => a.starts_with(&format!("{d}/")),
        Some(d) => a.strip_prefix(&format!("{d}/")).map(|rest| !rest.contains('/')).unwrap_or(false),
        None => false,
    }
}

/// `*`, an exact host, or `*.example.com` (sub-domains only).
pub fn host_pattern_matches(pattern: &str, host: &str) -> bool {
    let (p, h) = (pattern.trim().to_ascii_lowercase(), host.trim().to_ascii_lowercase());
    if p == "*" || p == h {
        return true;
    }
    p.strip_prefix("*.").map(|d| h.ends_with(&format!(".{d}"))).unwrap_or(false)
}

impl Grant {
    pub fn is_active(&self, now: &Timestamp) -> bool {
        !self.revoked
            && self.expires_at.map(|e| e > *now).unwrap_or(true)
            && self.max_uses.map(|m| self.used < m).unwrap_or(true)
    }

    /// Every recipient must be covered. Actions without recipients need `*` or no patterns.
    pub fn covers(&self, recipients: &[String]) -> bool {
        if recipients.is_empty() {
            return self.recipients.is_empty() || self.recipients.iter().any(|p| p == "*");
        }
        let matches = |p: &str, r: &str| match self.class {
            ActionClass::ExecCommand => command_pattern_matches(p, r),
            ActionClass::WriteFile => path_pattern_matches(p, r),
            ActionClass::FetchWeb => host_pattern_matches(p, r),
            _ => pattern_matches(p, r),
        };
        recipients.iter().all(|r| self.recipients.iter().any(|p| matches(p, r)))
    }
}

pub struct GrantStore<'a> {
    db: &'a Db,
}

const COLS: &str = "id, action_class, recipients, max_uses, used, allow_when_tainted, expires_at, created_at, revoked, note";

fn row_to_grant(r: &rusqlite::Row) -> rusqlite::Result<Grant> {
    let class: String = r.get(1)?;
    let recips: String = r.get(2)?;
    let exp: Option<String> = r.get(6)?;
    let created: String = r.get(7)?;
    Ok(Grant {
        id: r.get(0)?,
        class: ActionClass::parse(&class).unwrap_or(ActionClass::Payment),
        recipients: serde_json::from_str(&recips).unwrap_or_default(),
        max_uses: r.get(3)?,
        used: r.get(4)?,
        allow_when_tainted: r.get::<_, i64>(5)? != 0,
        expires_at: exp.and_then(|e| parse_rfc3339(&e).ok()),
        created_at: parse_rfc3339(&created).unwrap_or_default(),
        revoked: r.get::<_, i64>(8)? != 0,
        note: r.get(9)?,
    })
}

impl<'a> GrantStore<'a> {
    pub fn new(db: &'a Db) -> Self {
        GrantStore { db }
    }

    pub fn add(&self, g: &NewGrant, now: &Timestamp) -> Result<String> {
        if g.class == ActionClass::Payment {
            return Err(Error::Denied("payments can never be pre-approved".into()));
        }
        if g.class == ActionClass::SendMessage && g.recipients.is_empty() {
            return Err(Error::Invalid("a send grant must name recipients (address, *@domain or *)".into()));
        }
        let id = ids::random_id("grt");
        self.db
            .conn()
            .execute(
                &format!("INSERT INTO grants({COLS}) VALUES(?1, ?2, ?3, ?4, 0, ?5, ?6, ?7, 0, ?8)"),
                params![
                    id,
                    g.class.as_str(),
                    serde_json::to_string(&g.recipients)?,
                    g.max_uses,
                    g.allow_when_tainted as i64,
                    g.expires_at.map(|e| to_rfc3339(&e)),
                    to_rfc3339(now),
                    g.note
                ],
            )
            .db()?;
        self.db.audit("user", "grant_added", &format!("{id} class={} recipients={:?}", g.class.as_str(), g.recipients))?;
        Ok(id)
    }

    pub fn list(&self) -> Result<Vec<Grant>> {
        let conn = self.db.conn();
        let mut st = conn.prepare(&format!("SELECT {COLS} FROM grants ORDER BY created_at")).db()?;
        let rows = st.query_map([], row_to_grant).db()?;
        rows.collect::<rusqlite::Result<Vec<_>>>().db()
    }

    pub fn active_for(&self, class: ActionClass, now: &Timestamp) -> Result<Vec<Grant>> {
        Ok(self.list()?.into_iter().filter(|g| g.class == class && g.is_active(now)).collect())
    }

    pub fn revoke(&self, id: &str) -> Result<()> {
        let n = self.db.conn().execute("UPDATE grants SET revoked = 1 WHERE id = ?1", params![id]).db()?;
        if n == 0 {
            return Err(Error::NotFound(format!("grant {id}")));
        }
        self.db.audit("user", "grant_revoked", id)
    }

    /// Record one use of a grant.
    pub fn consume(&self, id: &str) -> Result<()> {
        self.db.conn().execute("UPDATE grants SET used = used + 1 WHERE id = ?1", params![id]).db()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn class_specific_patterns() {
        assert!(command_pattern_matches("cargo test*", "cargo test -p hatch-core"));
        assert!(command_pattern_matches("sh check.sh", "sh check.sh"));
        assert!(!command_pattern_matches("cargo test*", "cargo test && rm -rf /"));
        assert!(!command_pattern_matches("cargo test*", "cargo test $(curl x)"));
        assert!(!command_pattern_matches("sh check.sh", "sh check.sh; id"));
        assert!(path_pattern_matches("notes/*", "notes/a.md"));
        assert!(!path_pattern_matches("notes/*", "notes/sub/a.md"));
        assert!(path_pattern_matches("notes/**", "notes/sub/a.md"));
        assert!(!path_pattern_matches("notes/**", "notes/../secret"));
        assert!(path_pattern_matches("config/settings.ini", "./config/settings.ini"));
        assert!(host_pattern_matches("*.rust-lang.org", "doc.rust-lang.org"));
        assert!(!host_pattern_matches("*.rust-lang.org", "rust-lang.org.evil.io"));
        assert!(host_pattern_matches("docs.rs", "DOCS.rs"));
    }

    #[test]
    fn patterns() {
        assert!(pattern_matches("*@corp.com", "Bob@CORP.com"));
        assert!(!pattern_matches("*@corp.com", "bob@corp.com.evil.io"));
        assert!(pattern_matches("a@b.c", "A@B.C"));
        assert!(pattern_matches("*", "x@y.z"));
    }

    #[test]
    fn lifecycle() {
        let db = Db::open_in_memory().unwrap();
        let s = GrantStore::new(&db);
        let now = hatch_core::time::now();
        let g = NewGrant {
            class: ActionClass::SendMessage,
            recipients: vec!["*@corp.com".into()],
            max_uses: Some(1),
            allow_when_tainted: false,
            expires_at: Some(now + Duration::days(7)),
            note: "team replies".into(),
        };
        let id = s.add(&g, &now).unwrap();
        let active = s.active_for(ActionClass::SendMessage, &now).unwrap();
        assert_eq!(active.len(), 1);
        assert!(active[0].covers(&["a@corp.com".into()]));
        assert!(!active[0].covers(&["a@corp.com".into(), "x@evil.example".into()]));
        s.consume(&id).unwrap();
        assert!(s.active_for(ActionClass::SendMessage, &now).unwrap().is_empty());
        assert!(s.add(&NewGrant { class: ActionClass::Payment, ..g.clone() }, &now).is_err());
        assert!(s.add(&NewGrant { recipients: vec![], ..g.clone() }, &now).is_err());
        let id2 = s.add(&NewGrant { max_uses: None, ..g }, &now).unwrap();
        s.revoke(&id2).unwrap();
        assert!(s.active_for(ActionClass::SendMessage, &now).unwrap().is_empty());
        assert!(s.revoke("nope").is_err());
    }
}
