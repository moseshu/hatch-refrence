//! Browser sessions (one per tab) and their state machine, persisted so any
//! hatch process — and the user, via `hatch browser sessions` — can see them.
//!
//! ```text
//! Created ─► Navigating ─► Ready ⇄ Busy
//!                │           │ ▲      │
//!                ▼           ▼ │      ▼
//!            NeedsUser ──────┘ (resume after the user took over)
//! any state ─► Closed (final)
//! ```

use crate::error::{BResult, BrowserError};
use hatch_core::time::{parse_rfc3339, to_rfc3339, Timestamp};
use hatch_core::ids;
use hatch_store::{params, Db, DbResultExt, OptionalExtension};
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Created,
    Navigating,
    Ready,
    Busy,
    /// Waiting for the user: login wall or captcha. Only reading, screenshots
    /// and closing are allowed until `resume`.
    NeedsUser,
    Closed,
}

impl SessionState {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionState::Created => "created",
            SessionState::Navigating => "navigating",
            SessionState::Ready => "ready",
            SessionState::Busy => "busy",
            SessionState::NeedsUser => "needs_user",
            SessionState::Closed => "closed",
        }
    }

    pub fn parse(s: &str) -> SessionState {
        match s {
            "created" => SessionState::Created,
            "navigating" => SessionState::Navigating,
            "ready" => SessionState::Ready,
            "busy" => SessionState::Busy,
            "needs_user" => SessionState::NeedsUser,
            _ => SessionState::Closed,
        }
    }

    /// Is `self → to` a legal transition?
    pub fn can_go(self, to: SessionState) -> bool {
        use SessionState::*;
        match (self, to) {
            (Closed, _) => false,
            (_, Closed) => true,
            (Created, Navigating) => true,
            (Navigating, Ready | NeedsUser | Navigating) => true,
            (Ready, Busy | Navigating | NeedsUser | Ready) => true,
            (Busy, Ready | Navigating | NeedsUser) => true,
            (NeedsUser, Ready | NeedsUser) => true,
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionRecord {
    pub id: String,
    pub profile: String,
    pub origin: String,
    pub target_id: String,
    pub url: String,
    pub title: String,
    pub state: SessionState,
    pub reason: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

pub struct SessionStore<'a> {
    db: &'a Db,
}

const COLS: &str = "id, profile, origin, target_id, url, title, state, reason, created_at, updated_at";

fn row(r: &hatch_store::rusqlite::Row) -> hatch_store::rusqlite::Result<SessionRecord> {
    let ts = |i: usize| -> hatch_store::rusqlite::Result<Timestamp> {
        let s: String = r.get(i)?;
        Ok(parse_rfc3339(&s).unwrap_or_else(|_| hatch_core::time::now()))
    };
    let state: String = r.get(6)?;
    Ok(SessionRecord {
        id: r.get(0)?,
        profile: r.get(1)?,
        origin: r.get(2)?,
        target_id: r.get(3)?,
        url: r.get(4)?,
        title: r.get(5)?,
        state: SessionState::parse(&state),
        reason: r.get(7)?,
        created_at: ts(8)?,
        updated_at: ts(9)?,
    })
}

fn storage(e: hatch_core::Error) -> BrowserError {
    BrowserError::Unavailable(format!("会话存储：{e}"))
}

impl<'a> SessionStore<'a> {
    pub fn new(db: &'a Db) -> Self {
        SessionStore { db }
    }

    pub fn create(&self, profile: &str, origin: &str, target_id: &str, now: &Timestamp) -> BResult<String> {
        let id = ids::random_id("bs");
        let t = to_rfc3339(now);
        self.db
            .conn()
            .execute(
                &format!("INSERT INTO browser_sessions({COLS}) VALUES(?1,?2,?3,?4,'','',?5,'',?6,?6)"),
                params![id, profile, origin, target_id, SessionState::Created.as_str(), t],
            )
            .db()
            .map_err(storage)?;
        Ok(id)
    }

    pub fn get(&self, id: &str) -> BResult<SessionRecord> {
        self.db
            .conn()
            .query_row(&format!("SELECT {COLS} FROM browser_sessions WHERE id = ?1"), params![id], row)
            .optional()
            .db()
            .map_err(storage)?
            .ok_or_else(|| BrowserError::SessionUnavailable(format!("没有浏览器会话 {id}")))
    }

    /// Move to `to`, refusing illegal transitions.
    pub fn transition(&self, id: &str, to: SessionState, reason: &str, now: &Timestamp) -> BResult<()> {
        let cur = self.get(id)?;
        if !cur.state.can_go(to) {
            return Err(BrowserError::SessionUnavailable(format!("会话 {id} 处于 {}，不能变为 {}", cur.state.as_str(), to.as_str())));
        }
        self.db
            .conn()
            .execute(
                "UPDATE browser_sessions SET state = ?2, reason = ?3, updated_at = ?4 WHERE id = ?1",
                params![id, to.as_str(), reason, to_rfc3339(now)],
            )
            .db()
            .map_err(storage)?;
        Ok(())
    }

    pub fn set_page(&self, id: &str, url: &str, title: &str, now: &Timestamp) -> BResult<()> {
        self.db
            .conn()
            .execute(
                "UPDATE browser_sessions SET url = ?2, title = ?3, updated_at = ?4 WHERE id = ?1",
                params![id, url, title.chars().take(200).collect::<String>(), to_rfc3339(now)],
            )
            .db()
            .map_err(storage)?;
        Ok(())
    }

    pub fn list(&self, include_closed: bool) -> BResult<Vec<SessionRecord>> {
        let sql = if include_closed {
            format!("SELECT {COLS} FROM browser_sessions ORDER BY updated_at DESC LIMIT 200")
        } else {
            format!("SELECT {COLS} FROM browser_sessions WHERE state != 'closed' ORDER BY updated_at DESC")
        };
        let conn = self.db.conn();
        let mut st = conn.prepare(&sql).db().map_err(storage)?;
        let rows = st.query_map([], row).db().map_err(storage)?;
        rows.collect::<Result<Vec<_>, _>>().db().map_err(storage)
    }

    /// Mark every open session of `profile` closed (its browser exited).
    pub fn close_profile(&self, profile: &str, reason: &str, now: &Timestamp) -> BResult<usize> {
        self.db
            .conn()
            .execute(
                "UPDATE browser_sessions SET state = 'closed', reason = ?2, updated_at = ?3 WHERE profile = ?1 AND state != 'closed'",
                params![profile, reason, to_rfc3339(now)],
            )
            .db()
            .map_err(storage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_machine() {
        use SessionState::*;
        assert!(Created.can_go(Navigating) && Navigating.can_go(Ready) && Ready.can_go(Busy) && Busy.can_go(Ready));
        assert!(Navigating.can_go(NeedsUser) && NeedsUser.can_go(Ready));
        assert!(!NeedsUser.can_go(Busy), "a session waiting for the user cannot act");
        assert!(!Closed.can_go(Ready) && Ready.can_go(Closed));
        assert!(!Created.can_go(Ready));
    }

    #[test]
    fn persisted_transitions() {
        let db = Db::open_in_memory().unwrap();
        let s = SessionStore::new(&db);
        let now = hatch_core::time::now();
        let id = s.create("main", "chat:s1", "T1", &now).unwrap();
        s.transition(&id, SessionState::Navigating, "", &now).unwrap();
        s.transition(&id, SessionState::NeedsUser, "需要登录", &now).unwrap();
        assert!(s.transition(&id, SessionState::Busy, "", &now).is_err());
        s.set_page(&id, "http://x/login", "登录", &now).unwrap();
        let r = s.get(&id).unwrap();
        assert_eq!((r.state, r.reason.as_str(), r.url.as_str()), (SessionState::NeedsUser, "需要登录", "http://x/login"));
        assert_eq!(s.close_profile("main", "浏览器已退出", &now).unwrap(), 1);
        assert_eq!(s.get(&id).unwrap().state, SessionState::Closed);
        assert!(s.list(false).unwrap().is_empty());
        assert_eq!(s.list(true).unwrap().len(), 1);
        assert!(s.get("nope").is_err());
    }
}
