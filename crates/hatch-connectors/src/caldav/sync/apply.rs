//! The apply half of a sync run: server changes into the local DB,
//! and local changes onto the server. Kept in a separate file so
//! `sync/mod.rs` stays under the 400-line limit; these are `impl`
//! methods on [`super::SyncEngine`], not a separate type.

use super::{event_hash, server_updated, SyncEngine, SyncReport};
use crate::calendar::ics::to_ics;
use crate::calendar::CalendarRepo;
use crate::caldav::client::ServerEvent;
use crate::caldav::state::{CaldavState, Conflict, EvState};
use hatch_core::Result;

impl<'a> SyncEngine<'a> {
    pub(crate) fn apply_server_event(
        &self,
        se: &ServerEvent,
        cal_url: &str,
        dry_run: bool,
        at: &str,
        report: &mut SyncReport,
    ) -> Result<()> {
        let state = CaldavState::new(self.db);
        let cal = CalendarRepo::new(self.db);
        let Some(mut ev) = se.to_event(&self.tz)? else { return Ok(()) };
        ev.source = "caldav".into();
        let st = state.event_state(&se.uid)?;
        match st {
            None => {
                // First time seeing this UID from the server. If a local
                // event with the same UID already exists (e.g. from an
                // .ics import) and differs, that is a conflict too —
                // never silently overwrite an unsynced local event.
                let local = cal.all()?.into_iter().find(|e| e.uid == se.uid);
                let collides = local
                    .as_ref()
                    .is_some_and(|e| event_hash(e) != event_hash(&ev));
                if collides {
                    let conflict = Conflict {
                        uid: se.uid.clone(),
                        calendar_url: cal_url.into(),
                        kind: "edit".into(),
                        local_updated_at: at.to_string(),
                        server_updated_at: se.ics.as_deref().map(server_updated).unwrap_or_default(),
                        kept: "server".into(),
                        local_snapshot: serde_json::to_string(&local.unwrap()).unwrap_or_default(),
                        status: "open".into(),
                        created_at: at.to_string(),
                        resolved_at: None,
                    };
                    report.conflicts_new.push(conflict.clone());
                    report.pulled_new += 1;
                    if dry_run {
                        return Ok(());
                    }
                    state.record_conflict(&conflict)?;
                    cal.upsert(&ev)?;
                    state.set_event_state(
                        &se.uid,
                        &EvState {
                            calendar_url: cal_url.into(),
                            etag: se.etag.clone(),
                            local_hash: event_hash(&ev),
                            conflicted: true,
                        },
                    )?;
                    return Ok(());
                }
                report.pulled_new += 1;
                if dry_run {
                    return Ok(());
                }
                cal.upsert(&ev)?;
                state.set_event_state(
                    &se.uid,
                    &EvState {
                        calendar_url: cal_url.into(),
                        etag: se.etag.clone(),
                        local_hash: event_hash(&ev),
                        conflicted: false,
                    },
                )?;
            }
            Some(prev) if prev.etag == se.etag => {}
            Some(prev) => {
                // Server changed it. Did the local side change too?
                let local = cal.all()?.into_iter().find(|e| e.uid == se.uid);
                let local_dirty = match &local {
                    Some(e) => event_hash(e) != prev.local_hash,
                    None => true, // locally deleted while server edited it
                };
                if !local_dirty {
                    report.pulled_updated += 1;
                    if dry_run {
                        return Ok(());
                    }
                    cal.upsert(&ev)?;
                    state.set_event_state(
                        &se.uid,
                        &EvState {
                            calendar_url: cal_url.into(),
                            etag: se.etag.clone(),
                            local_hash: event_hash(&ev),
                            conflicted: false,
                        },
                    )?;
                    return Ok(());
                }
                // Conflict: server wins, local version recorded.
                let (kind, snapshot) = match &local {
                    Some(e) => ("edit", serde_json::to_string(e).unwrap_or_default()),
                    None => ("local_deleted", "__deleted__".to_string()),
                };
                let conflict = Conflict {
                    uid: se.uid.clone(),
                    calendar_url: cal_url.into(),
                    kind: kind.into(),
                    local_updated_at: at.to_string(),
                    server_updated_at: se.ics.as_deref().map(server_updated).unwrap_or_default(),
                    kept: "server".into(),
                    local_snapshot: snapshot,
                    status: "open".into(),
                    created_at: at.to_string(),
                    resolved_at: None,
                };
                report.conflicts_new.push(conflict.clone());
                report.pulled_updated += 1;
                if dry_run {
                    return Ok(());
                }
                state.record_conflict(&conflict)?;
                cal.upsert(&ev)?;
                state.set_event_state(
                    &se.uid,
                    &EvState {
                        calendar_url: cal_url.into(),
                        etag: se.etag.clone(),
                        local_hash: event_hash(&ev),
                        conflicted: true,
                    },
                )?;
            }
        }
        Ok(())
    }

    pub(crate) fn apply_server_delete(
        &self,
        uid: &str,
        cal_url: &str,
        dry_run: bool,
        at: &str,
        report: &mut SyncReport,
    ) -> Result<()> {
        let state = CaldavState::new(self.db);
        let cal = CalendarRepo::new(self.db);
        let Some(prev) = state.event_state(uid)? else { return Ok(()) };
        let local = cal.all()?.into_iter().find(|e| e.uid == uid);
        let local_dirty = match &local {
            Some(e) => event_hash(e) != prev.local_hash,
            None => false, // already deleted locally: agree
        };
        if local_dirty {
            // Server deleted, local edited: server wins, local copy
            // stays in the local DB until the conflict is resolved.
            let conflict = Conflict {
                uid: uid.to_string(),
                calendar_url: cal_url.into(),
                kind: "server_deleted".into(),
                local_updated_at: at.to_string(),
                server_updated_at: at.to_string(),
                kept: "server".into(),
                local_snapshot: serde_json::to_string(&local.unwrap()).unwrap_or_default(),
                status: "open".into(),
                created_at: at.to_string(),
                resolved_at: None,
            };
            report.conflicts_new.push(conflict.clone());
            if dry_run {
                return Ok(());
            }
            state.record_conflict(&conflict)?;
            state.set_event_state(
                uid,
                &EvState {
                    calendar_url: cal_url.into(),
                    etag: prev.etag,
                    local_hash: prev.local_hash,
                    conflicted: true,
                },
            )?;
            return Ok(());
        }
        report.pulled_deleted += 1;
        if dry_run {
            return Ok(());
        }
        cal.delete(uid)?;
        state.delete_event_state(uid)?;
        Ok(())
    }

    pub(crate) fn push_local(
        &self,
        cal_url: &str,
        primary: &str,
        dry_run: bool,
        report: &mut SyncReport,
    ) -> Result<()> {
        let state = CaldavState::new(self.db);
        let cal = CalendarRepo::new(self.db);
        let locals = cal.all()?;
        let states = state.all_event_states()?;
        let by_uid: std::collections::HashMap<&str, &EvState> =
            states.iter().map(|(u, s)| (u.as_str(), s)).collect();
        // Only push to this calendar: events whose state says so, plus
        // brand-new local events (they go to the primary calendar).
        for e in &locals {
            match by_uid.get(e.uid.as_str()) {
                None => {
                    if cal_url != primary {
                        continue;
                    }
                    report.pushed_created += 1;
                    if dry_run {
                        continue;
                    }
                    let etag = self.client.put_event(cal_url, &e.uid, &to_ics(e), None)?;
                    state.set_event_state(
                        &e.uid,
                        &EvState {
                            calendar_url: cal_url.into(),
                            etag,
                            local_hash: event_hash(e),
                            conflicted: false,
                        },
                    )?;
                }
                Some(prev) => {
                    if prev.conflicted || prev.calendar_url != cal_url {
                        continue;
                    }
                    if event_hash(e) == prev.local_hash {
                        continue;
                    }
                    report.pushed_updated += 1;
                    if dry_run {
                        continue;
                    }
                    let etag = self.client.put_event(
                        cal_url,
                        &e.uid,
                        &to_ics(e),
                        if prev.etag.is_empty() { None } else { Some(prev.etag.as_str()) },
                    )?;
                    state.set_event_state(
                        &e.uid,
                        &EvState {
                            calendar_url: cal_url.into(),
                            etag,
                            local_hash: event_hash(e),
                            conflicted: false,
                        },
                    )?;
                }
            }
        }
        // Locally deleted events: state exists, event is gone.
        let local_uids: std::collections::HashSet<&str> =
            locals.iter().map(|e| e.uid.as_str()).collect();
        for (uid, prev) in &states {
            if prev.conflicted || prev.calendar_url != cal_url || local_uids.contains(uid.as_str())
            {
                continue;
            }
            report.pushed_deleted += 1;
            if dry_run {
                continue;
            }
            self.client.delete_event(
                cal_url,
                uid,
                if prev.etag.is_empty() { None } else { Some(prev.etag.as_str()) },
            )?;
            state.delete_event_state(uid)?;
        }
        Ok(())
    }

}
