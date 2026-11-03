//! Bidirectional CalDAV sync engine.
//!
//! # Conflict rule (the whole point — read this first)
//!
//! * **Server wins on conflict.** When the same UID changed on both
//!   sides since the last sync, the server version is applied locally,
//!   the conflict is **recorded** (UID, both versions' updated times,
//!   what was kept), and the local version is kept in the conflict
//!   record — never silently dropped.
//! * The local version is pushed **only** after the user explicitly
//!   approves it: `hatch calendar resolve <uid> --keep local`. The
//!   engine never compares timestamps to auto-prefer local: "newer"
//!   is information shown to the user, approval is the gate.
//! * Deletion conflicts follow the same rule: server-deleted vs
//!   locally-edited → server wins, the local copy stays in the local
//!   DB as a conflict record until resolved; locally-deleted vs
//!   server-edited → the server version is restored locally and the
//!   deletion is recorded.
//!
//! # How a sync run works
//!
//! 1. Pull server changes since the stored sync-token (RFC 6578
//!    sync-collection); full fetch over a wide range when there is no
//!    token yet, or when the server does not support sync-collection.
//! 2. Apply to `calendar_events`, recording conflicts as above.
//! 3. Push local changes (created / edited / deleted since last sync,
//!    detected by comparing a stable hash of each local event against
//!    the hash stored at the last sync).
//! 4. Store the new sync-token. A re-run with no changes makes no
//!    PUT/DELETE calls (idempotent).
//!
//! Local events with no sync state (e.g. from `hatch calendar import`)
//! are pushed to the primary calendar: the configured `calendar_url`,
//! or the first discovered collection.

mod apply;

use super::client::{CaldavClient, ServerEvent};
use super::state::{CaldavState, CalState, Conflict, Keep};
use crate::calendar::Event;
use chrono::{Duration, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use hatch_core::time::{now, to_rfc3339};
use hatch_core::{Error, Result};
use hatch_store::Db;
use sha2::Digest;

/// Sync settings (built from `[caldav]` config by the CLI).
#[derive(Clone, Debug)]
pub struct SyncSettings {
    pub server_url: String,
    pub username: String,
    pub token_ref: String,
    /// Explicit collection URL; `None` = discover via PROPFIND.
    pub calendar_url: Option<String>,
    /// Days back / forward for a full (no-token) fetch.
    pub range_back_days: i64,
    pub range_forward_days: i64,
}

impl SyncSettings {
    pub fn new(server_url: &str, username: &str, token_ref: &str) -> Self {
        SyncSettings {
            server_url: server_url.to_string(),
            username: username.to_string(),
            token_ref: token_ref.to_string(),
            calendar_url: None,
            range_back_days: 365,
            range_forward_days: 730,
        }
    }
}

/// Stable hash of an event's synced fields. sha256 (not the default
/// hasher — that one is randomly seeded per process and would not
/// survive a restart).
pub fn event_hash(e: &Event) -> String {
    let mut h = sha2::Sha256::new();
    for part in [&e.uid, &e.title, &to_rfc3339(&e.starts_at), &to_rfc3339(&e.ends_at), &e.location]
    {
        h.update(part.as_bytes());
        h.update([0u8]);
    }
    hex::encode(h.finalize())
}

/// Server-side updated time from an .ics body: LAST-MODIFIED, else
/// DTSTAMP, else empty (unknown — never invented). LAST-MODIFIED wins
/// even when DTSTAMP comes first in the body.
pub fn server_updated(ics: &str) -> String {
    let mut dtstamp = String::new();
    for line in ics.lines() {
        let line = line.trim_end_matches('\r');
        let parse = |v: &str| {
            NaiveDateTime::parse_from_str(v.trim(), "%Y%m%dT%H%M%SZ")
                .ok()
                .map(|n| to_rfc3339(&Utc.from_utc_datetime(&n)))
        };
        if let Some(v) = line.strip_prefix("LAST-MODIFIED:") {
            if let Some(t) = parse(v) {
                return t;
            }
        }
        if dtstamp.is_empty() {
            if let Some(v) = line.strip_prefix("DTSTAMP:") {
                if let Some(t) = parse(v) {
                    dtstamp = t;
                }
            }
        }
    }
    dtstamp
}

#[derive(Clone, Debug, Default)]
pub struct SyncReport {
    pub dry_run: bool,
    pub pulled_new: usize,
    pub pulled_updated: usize,
    pub pulled_deleted: usize,
    pub pushed_created: usize,
    pub pushed_updated: usize,
    pub pushed_deleted: usize,
    pub conflicts_new: Vec<Conflict>,
    pub errors: Vec<String>,
    pub calendars: Vec<String>,
}

impl SyncReport {
    pub fn summary(&self) -> String {
        let mut s = format!(
            "CalDAV 同步{}：拉取新增 {}、更新 {}、删除 {}；推送新增 {}、更新 {}、删除 {}。",
            if self.dry_run { "（试运行，未写入）" } else { "" },
            self.pulled_new, self.pulled_updated, self.pulled_deleted,
            self.pushed_created, self.pushed_updated, self.pushed_deleted
        );
        if !self.conflicts_new.is_empty() {
            s.push_str(&format!(
                "\n发现 {} 个冲突（服务器版本已保留）：{}。用 `hatch calendar conflicts` 查看，`hatch calendar resolve <uid> --keep local|server` 处理。",
                self.conflicts_new.len(),
                self.conflicts_new.iter().map(|c| c.uid.as_str()).collect::<Vec<_>>().join(", ")
            ));
        }
        for e in &self.errors {
            s.push_str(&format!("\n注意：{e}"));
        }
        s
    }
}

/// TurnOutcome text for a sync report (relay path — see CLI).
pub fn report_outcome_text(report: &SyncReport) -> String {
    report.summary()
}

/// The sync engine: one [`CaldavClient`] + local store.
pub struct SyncEngine<'a> {
    db: &'a Db,
    tz: Tz,
    client: CaldavClient,
    settings: SyncSettings,
}

impl<'a> SyncEngine<'a> {
    pub fn new(db: &'a Db, tz: Tz, client: CaldavClient, settings: SyncSettings) -> Self {
        SyncEngine { db, tz, client, settings }
    }

    /// Calendars to sync: the configured URL, or discovery.
    fn calendars(&self) -> Result<Vec<(String, String)>> {
        if let Some(url) = &self.settings.calendar_url {
            return Ok(vec![(url.clone(), String::new())]);
        }
        Ok(self
            .client
            .discover()?
            .into_iter()
            .map(|c| (c.url, c.display_name))
            .collect())
    }

    /// Full sync run: pull → apply → push → store token.
    pub fn sync(&self, dry_run: bool) -> Result<SyncReport> {
        let cals = self.calendars()?;
        if cals.is_empty() {
            return Err(Error::Connector(
                "CalDAV 发现不到任何日历集合：检查服务器 URL 与账号权限".into(),
            ));
        }
        let mut report = SyncReport { dry_run, ..Default::default() };
        let primary = cals[0].0.clone();
        for (url, display) in &cals {
            report.calendars.push(url.clone());
            self.sync_calendar(url, display, &primary, dry_run, &mut report)?;
        }
        Ok(report)
    }

    fn sync_calendar(
        &self,
        cal_url: &str,
        display: &str,
        primary: &str,
        dry_run: bool,
        report: &mut SyncReport,
    ) -> Result<()> {
        let state = CaldavState::new(self.db);
        let prev = state.calendar(cal_url)?.unwrap_or(CalState {
            sync_token: String::new(),
            ctag: String::new(),
            last_sync_at: None,
        });
        // Pull: incremental when we have a token, full range otherwise.
        // A server that rejects sync-collection falls back to a full
        // fetch (etag comparison still makes the apply a no-op).
        let (changed, deleted, new_token) = match self.pull(cal_url, &prev.sync_token) {
            Ok(v) => v,
            Err(e) => {
                report.errors.push(format!("{cal_url} 增量同步失败，改用全量拉取：{e}"));
                let now = now();
                let evs = self.client.fetch_range(
                    cal_url,
                    &(now - Duration::days(self.settings.range_back_days)),
                    &(now + Duration::days(self.settings.range_forward_days)),
                )?;
                (evs, Vec::new(), String::new())
            }
        };
        let at = to_rfc3339(&now());
        for se in &changed {
            self.apply_server_event(se, cal_url, dry_run, &at, report)?;
        }
        for uid in &deleted {
            self.apply_server_delete(uid, cal_url, dry_run, &at, report)?;
        }
        self.push_local(cal_url, primary, dry_run, report)?;
        if !dry_run {
            state.set_calendar(cal_url, display, &new_token, "", &at)?;
        }
        Ok(())
    }

    fn pull(
        &self,
        cal_url: &str,
        token: &str,
    ) -> Result<(Vec<ServerEvent>, Vec<String>, String)> {
        let token = if token.is_empty() { None } else { Some(token) };
        self.client.sync_collection(cal_url, token)
    }

    /// Resolve an open conflict (no network: see [`CaldavState::resolve`]).
    pub fn resolve(&self, uid: &str, keep: Keep) -> Result<String> {
        CaldavState::new(self.db).resolve(uid, keep)
    }

    /// Status (no network: see [`CaldavState::status`]).
    pub fn status(&self) -> Result<SyncStatus> {
        CaldavState::new(self.db).status(&self.settings)
    }
}

/// `hatch calendar status` data (no network involved).
pub struct SyncStatus {
    pub server_url: String,
    pub username: String,
    pub token_ref: String,
    pub calendars: Vec<(String, CalState)>,
    pub pending_new: usize,
    pub pending_edit: usize,
    pub pending_delete: usize,
    pub open_conflicts: usize,
}

impl SyncStatus {
    pub fn has_token(&self) -> bool {
        self.calendars.iter().any(|(_, c)| !c.sync_token.is_empty())
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
    fn hash_is_stable_and_change_sensitive() {
        let a = ev("u1");
        let mut b = ev("u1");
        assert_eq!(event_hash(&a), event_hash(&b));
        b.title = "changed".into();
        assert_ne!(event_hash(&a), event_hash(&b));
    }

    #[test]
    fn server_updated_prefers_last_modified() {
        let ics = "BEGIN:VEVENT\nUID:u\nDTSTAMP:20261101T000000Z\nLAST-MODIFIED:20261102T030405Z\nEND:VEVENT";
        assert_eq!(server_updated(ics), "2026-11-02T03:04:05Z");
        assert_eq!(server_updated("BEGIN:VEVENT\nDTSTAMP:20261101T000000Z\nEND:VEVENT"), "2026-11-01T00:00:00Z");
        assert_eq!(server_updated("BEGIN:VEVENT\nEND:VEVENT"), "");
    }
}
