//! Sync state: the three `caldav_*` tables, conflict records, and the
//! offline operations on them (`resolve`, `status`). Nothing here
//! touches the network: resolving a conflict only rewrites local rows
//! so the *next* sync pushes the chosen version (see [`super::sync`]).

use super::sync::{event_hash, SyncSettings, SyncStatus};
use crate::calendar::{CalendarRepo, Event};
use hatch_core::time::{now, to_rfc3339};
use hatch_core::{Error, Result};
use hatch_store::{params, rusqlite, Db, DbResultExt};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Conflict {
    pub uid: String,
    pub calendar_url: String,
    /// `edit` | `server_deleted` | `local_deleted`.
    pub kind: String,
    pub local_updated_at: String,
    pub server_updated_at: String,
    /// `server` | `local`.
    pub kept: String,
    /// JSON of the local event, or the literal `__deleted__`.
    pub local_snapshot: String,
    /// `open` | `resolved`.
    pub status: String,
    pub created_at: String,
    pub resolved_at: Option<String>,
}

impl Conflict {
    /// The local event as it was when the conflict was recorded
    /// (`None` when the local side had deleted the event).
    pub fn local_event(&self) -> Option<Event> {
        if self.local_snapshot == "__deleted__" {
            return None;
        }
        serde_json::from_str(&self.local_snapshot).ok()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keep {
    Local,
    Server,
}

pub struct CalState {
    pub sync_token: String,
    pub ctag: String,
    pub last_sync_at: Option<String>,
}

#[derive(Clone, Debug)]
pub struct EvState {
    pub calendar_url: String,
    pub etag: String,
    pub local_hash: String,
    pub conflicted: bool,
}

/// Sync-state repository: the three `caldav_*` tables.
pub struct CaldavState<'a> {
    db: &'a Db,
}

impl<'a> CaldavState<'a> {
    pub fn new(db: &'a Db) -> Self {
        CaldavState { db }
    }

    pub fn calendar(&self, url: &str) -> Result<Option<CalState>> {
        let row = self.db.conn().query_row(
            "SELECT sync_token, ctag, last_sync_at FROM caldav_calendars WHERE url = ?1",
            params![url],
            |r| {
                Ok(CalState {
                    sync_token: r.get(0)?,
                    ctag: r.get(1)?,
                    last_sync_at: r.get(2)?,
                })
            },
        );
        match row {
            Ok(c) => Ok(Some(c)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(Error::Storage(e.to_string())),
        }
    }

    pub fn set_calendar(&self, url: &str, display: &str, token: &str, ctag: &str, at: &str) -> Result<()> {
        self.db
            .conn()
            .execute(
                "INSERT INTO caldav_calendars(url, display_name, sync_token, ctag, last_sync_at)
                 VALUES(?1,?2,?3,?4,?5)
                 ON CONFLICT(url) DO UPDATE SET display_name=excluded.display_name,
                 sync_token=excluded.sync_token, ctag=excluded.ctag, last_sync_at=excluded.last_sync_at",
                params![url, display, token, ctag, at],
            )
            .db()?;
        Ok(())
    }

    pub fn list_calendars(&self) -> Result<Vec<(String, CalState)>> {
        let conn = self.db.conn();
        let mut st = conn
            .prepare("SELECT url, sync_token, ctag, last_sync_at FROM caldav_calendars ORDER BY url")
            .db()?;
        let rows = st
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    CalState {
                        sync_token: r.get(1)?,
                        ctag: r.get(2)?,
                        last_sync_at: r.get(3)?,
                    },
                ))
            })
            .db()?;
        rows.collect::<rusqlite::Result<Vec<_>>>().db()
    }

    pub fn event_state(&self, uid: &str) -> Result<Option<EvState>> {
        let row = self.db.conn().query_row(
            "SELECT calendar_url, etag, local_hash, conflicted FROM caldav_event_state WHERE uid = ?1",
            params![uid],
            |r| {
                Ok(EvState {
                    calendar_url: r.get(0)?,
                    etag: r.get(1)?,
                    local_hash: r.get(2)?,
                    conflicted: r.get::<_, i64>(3)? != 0,
                })
            },
        );
        match row {
            Ok(s) => Ok(Some(s)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(Error::Storage(e.to_string())),
        }
    }

    pub fn set_event_state(&self, uid: &str, st: &EvState) -> Result<()> {
        self.db
            .conn()
            .execute(
                "INSERT INTO caldav_event_state(uid, calendar_url, etag, local_hash, conflicted)
                 VALUES(?1,?2,?3,?4,?5)
                 ON CONFLICT(uid) DO UPDATE SET calendar_url=excluded.calendar_url,
                 etag=excluded.etag, local_hash=excluded.local_hash, conflicted=excluded.conflicted",
                params![uid, st.calendar_url, st.etag, st.local_hash, i64::from(st.conflicted)],
            )
            .db()?;
        Ok(())
    }

    pub fn delete_event_state(&self, uid: &str) -> Result<()> {
        self.db.conn().execute("DELETE FROM caldav_event_state WHERE uid = ?1", params![uid]).db()?;
        Ok(())
    }

    pub fn all_event_states(&self) -> Result<Vec<(String, EvState)>> {
        let conn = self.db.conn();
        let mut st = conn
            .prepare("SELECT uid, calendar_url, etag, local_hash, conflicted FROM caldav_event_state ORDER BY uid")
            .db()?;
        let rows = st
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    EvState {
                        calendar_url: r.get(1)?,
                        etag: r.get(2)?,
                        local_hash: r.get(3)?,
                        conflicted: r.get::<_, i64>(4)? != 0,
                    },
                ))
            })
            .db()?;
        rows.collect::<rusqlite::Result<Vec<_>>>().db()
    }

    pub fn record_conflict(&self, c: &Conflict) -> Result<()> {
        self.db
            .conn()
            .execute(
                "INSERT INTO caldav_conflicts(uid, calendar_url, kind, local_updated_at,
                 server_updated_at, kept, local_snapshot, status, created_at, resolved_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
                 ON CONFLICT(uid) DO UPDATE SET calendar_url=excluded.calendar_url,
                 kind=excluded.kind, local_updated_at=excluded.local_updated_at,
                 server_updated_at=excluded.server_updated_at, kept=excluded.kept,
                 local_snapshot=excluded.local_snapshot, status=excluded.status,
                 created_at=excluded.created_at, resolved_at=excluded.resolved_at",
                params![
                    c.uid,
                    c.calendar_url,
                    c.kind,
                    c.local_updated_at,
                    c.server_updated_at,
                    c.kept,
                    c.local_snapshot,
                    c.status,
                    c.created_at,
                    c.resolved_at
                ],
            )
            .db()?;
        Ok(())
    }

    pub fn list_conflicts(&self, open_only: bool) -> Result<Vec<Conflict>> {
        let sql = if open_only {
            "SELECT uid, calendar_url, kind, local_updated_at, server_updated_at, kept, local_snapshot, status, created_at, resolved_at FROM caldav_conflicts WHERE status = 'open' ORDER BY created_at"
        } else {
            "SELECT uid, calendar_url, kind, local_updated_at, server_updated_at, kept, local_snapshot, status, created_at, resolved_at FROM caldav_conflicts ORDER BY created_at"
        };
        let conn = self.db.conn();
        let mut st = conn.prepare(sql).db()?;
        let rows = st
            .query_map([], |r| {
                Ok(Conflict {
                    uid: r.get(0)?,
                    calendar_url: r.get(1)?,
                    kind: r.get(2)?,
                    local_updated_at: r.get(3)?,
                    server_updated_at: r.get(4)?,
                    kept: r.get(5)?,
                    local_snapshot: r.get(6)?,
                    status: r.get(7)?,
                    created_at: r.get(8)?,
                    resolved_at: r.get(9)?,
                })
            })
            .db()?;
        rows.collect::<rusqlite::Result<Vec<_>>>().db()
    }

    pub fn get_conflict(&self, uid: &str) -> Result<Option<Conflict>> {
        Ok(self.list_conflicts(false)?.into_iter().find(|c| c.uid == uid))
    }

    /// Clear the conflicted flag on an event's state row (if any).
    fn unconflict(&self, uid: &str) -> Result<()> {
        if let Some(mut st) = self.event_state(uid)? {
            st.conflicted = false;
            self.set_event_state(uid, &st)?;
        }
        Ok(())
    }

    /// Resolve an open conflict. `--keep local` restores the recorded
    /// local version (or the pending deletion) so the *next* sync
    /// pushes it; `--keep server` accepts the server version that is
    /// already in place (and deletes the local copy when the server
    /// had deleted the event). Nothing is pushed by resolve itself.
    pub fn resolve(&self, uid: &str, keep: Keep) -> Result<String> {
        let cal = CalendarRepo::new(self.db);
        let Some(c) = self.get_conflict(uid)? else {
            return Err(Error::NotFound(format!("没有 UID 为 {uid} 的冲突记录")));
        };
        if c.status != "open" {
            return Err(Error::Invalid(format!("冲突 {uid} 已处理（{}）", c.kept)));
        }
        let at = to_rfc3339(&now());
        let mut c = c;
        match (c.kind.as_str(), keep) {
            ("edit", Keep::Local) => {
                if let Some(e) = c.local_event() {
                    cal.upsert(&e)?;
                }
                self.unconflict(uid)?;
            }
            ("edit", Keep::Server) => {
                self.unconflict(uid)?;
            }
            ("server_deleted", Keep::Local) => {
                // Local copy stays; clear etag + stored hash so the
                // next push re-creates it (If-None-Match *) instead of
                // skipping a hash-matching event or If-Matching a
                // deleted resource.
                if let Some(mut st) = self.event_state(uid)? {
                    st.conflicted = false;
                    st.etag = String::new();
                    st.local_hash = String::new();
                    self.set_event_state(uid, &st)?;
                }
            }
            ("server_deleted", Keep::Server) => {
                cal.delete(uid)?;
                self.delete_event_state(uid)?;
            }
            ("local_deleted", Keep::Local) => {
                // The deletion stands: re-apply it locally (sync had
                // restored the server version); the state row stays so
                // the next sync DELETEs it on the server.
                cal.delete(uid)?;
                self.unconflict(uid)?;
            }
            ("local_deleted", Keep::Server) => {
                self.unconflict(uid)?;
            }
            _ => {
                return Err(Error::Invalid(format!("未知的冲突类型 {}", c.kind)));
            }
        }
        c.status = "resolved".into();
        c.kept = match keep {
            Keep::Local => "local".into(),
            Keep::Server => "server".into(),
        };
        c.resolved_at = Some(at);
        self.record_conflict(&c)?;
        Ok(match keep {
            Keep::Local => format!(
                "冲突 {uid} 已按本地版本处理：下次同步会把它推送到服务器。"
            ),
            Keep::Server => format!("冲突 {uid} 已按服务器版本处理。"),
        })
    }


    /// Status for `hatch calendar status` / `hatch doctor` (no network):
    /// server URL, per-calendar last sync + token presence, pending
    /// local changes (new / edited / deleted since last sync).
    pub fn status(&self, settings: &SyncSettings) -> Result<SyncStatus> {
        let cal = CalendarRepo::new(self.db);
        let locals = cal.all()?;
        let states = self.all_event_states()?;
        let by_uid: std::collections::HashMap<&str, &EvState> =
            states.iter().map(|(u, s)| (u.as_str(), s)).collect();
        let mut pending_new = 0;
        let mut pending_edit = 0;
        for e in &locals {
            match by_uid.get(e.uid.as_str()) {
                None => pending_new += 1,
                Some(s) if !s.conflicted && event_hash(e) != s.local_hash => pending_edit += 1,
                _ => {}
            }
        }
        let local_uids: std::collections::HashSet<&str> =
            locals.iter().map(|e| e.uid.as_str()).collect();
        let pending_delete =
            states.iter().filter(|(u, s)| !s.conflicted && !local_uids.contains(u.as_str())).count();
        Ok(SyncStatus {
            server_url: settings.server_url.clone(),
            username: settings.username.clone(),
            token_ref: settings.token_ref.clone(),
            calendars: self.list_calendars()?,
            pending_new,
            pending_edit,
            pending_delete,
            open_conflicts: self.list_conflicts(true)?.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatch_core::time::parse_rfc3339;

    fn ev(uid: &str) -> Event {
        Event {
            uid: uid.into(),
            title: "T".into(),
            starts_at: parse_rfc3339("2026-11-04T02:00:00Z").unwrap(),
            ends_at: parse_rfc3339("2026-11-04T03:00:00Z").unwrap(),
            location: String::new(),
            source: "test".into(),
        }
    }

    #[test]
    fn conflict_record_round_trip() {
        let db = Db::open_in_memory().unwrap();
        let st = CaldavState::new(&db);
        let c = Conflict {
            uid: "u1".into(),
            calendar_url: "http://127.0.0.1:8090/cal/".into(),
            kind: "edit".into(),
            local_updated_at: "2026-11-03T00:00:00Z".into(),
            server_updated_at: "2026-11-03T01:00:00Z".into(),
            kept: "server".into(),
            local_snapshot: serde_json::to_string(&ev("u1")).unwrap(),
            status: "open".into(),
            created_at: "2026-11-03T02:00:00Z".into(),
            resolved_at: None,
        };
        st.record_conflict(&c).unwrap();
        let got = st.list_conflicts(true).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].local_event().unwrap().uid, "u1");
        assert!(st.get_conflict("missing").unwrap().is_none());
    }
}
