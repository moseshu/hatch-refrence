//! The Google Calendar event executor: the twin of
//! [`CreateEventExecutor`](super::executors::CreateEventExecutor),
//! kept in its own file so `executors.rs` stays under 400 lines.
//!
//! Same gate, same slot re-check, but the event goes to the real
//! Google Calendar (primary) instead of the local store, and the
//! Google event id is the receipt. Only ever constructed for a
//! Google target, and only ever invoked through the approval-gated
//! `create_google_event` kind — an unapproved create never reaches
//! the helper.

use super::executors::{ActionExecutor, EventPayload, ExecContext};
use hatch_connectors::calendar::google::{GoogleCalendarWriter, NewGoogleEvent};
use hatch_core::{Error, Result};
use serde_json::Value;
use std::sync::Arc;

pub struct CreateGoogleEventExecutor {
    writer: Arc<dyn GoogleCalendarWriter>,
}

impl CreateGoogleEventExecutor {
    pub fn new(writer: Arc<dyn GoogleCalendarWriter>) -> Self {
        CreateGoogleEventExecutor { writer }
    }
}

impl ActionExecutor for CreateGoogleEventExecutor {
    fn kind(&self) -> &'static str {
        "create_google_event"
    }

    fn execute(&self, payload: &Value, _ctx: &ExecContext) -> Result<String> {
        let p: EventPayload = serde_json::from_value(payload.clone())?;
        if p.calendar != "google" {
            return Err(Error::Invalid(
                "create_google_event 只处理 Google 日历事件".into(),
            ));
        }
        // Slot re-check against Google (after approval, right before
        // the effect): the calendar may have changed since the
        // preview. Any overlapping event refuses the create instead
        // of double-booking — the user picks another time.
        if let Some(e) = self
            .writer
            .list(p.starts_at, p.ends_at)?
            .into_iter()
            .find(|e| e.overlaps(&p.starts_at, &p.ends_at))
        {
            return Err(Error::Invalid(format!(
                "时段冲突：{}–{} 已有日程《{}》，不会重复建日程；请另选时间",
                hatch_core::time::to_rfc3339(&e.start),
                hatch_core::time::to_rfc3339(&e.end),
                e.title
            )));
        }
        let created = self.writer.insert(&NewGoogleEvent {
            title: p.title.clone(),
            start: p.starts_at,
            end: p.ends_at,
            location: p.location.clone(),
            attendees: p.attendees.clone(),
        })?;
        // Read the event back: the Google id is the receipt, and the
        // read confirms it really landed on the primary calendar
        // with the approved content. A mismatch is a definite
        // failure — the insert already happened, so the message must
        // say to check, never to retry blindly.
        let verified = self.writer.get(&created.id)?;
        if verified.id != created.id
            || verified.title != created.title
            || verified.start != created.start
        {
            return Err(Error::Connector(
                "Google 日历写入后读回的事件与刚创建的不一致，请核对".into(),
            ));
        }
        Ok(format!(
            "event {} created ({})",
            verified.id, verified.title
        ))
    }
}
