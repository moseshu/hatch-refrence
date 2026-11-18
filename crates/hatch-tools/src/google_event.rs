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
    fn verify(
        &self,
        payload: &Value,
        receipt: &str,
        _ctx: &ExecContext,
    ) -> Result<crate::verification::EvidenceCheck> {
        let p: EventPayload = serde_json::from_value(payload.clone())?;
        let id = receipt
            .strip_prefix("event ")
            .and_then(|s| s.split_whitespace().next())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Invalid("日历回执缺少事件 ID".into()))?;
        let e = self.writer.get(id)?;
        let expected = serde_json::json!([
            id,
            p.title,
            p.starts_at,
            p.ends_at,
            p.location,
            crate::evidence_readers::addresses(p.attendees)
        ]);
        let observed = serde_json::json!([
            e.id,
            e.title,
            e.start,
            e.end,
            e.location,
            crate::evidence_readers::addresses(e.attendees)
        ]);
        Ok(crate::verification::EvidenceCheck::compare(
            "google-event-v1",
            "google_primary_get",
            id,
            expected.to_string().as_bytes(),
            observed.to_string().as_bytes(),
        ))
    }

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
        let created = self
            .writer
            .insert(&NewGoogleEvent {
                title: p.title.clone(),
                start: p.starts_at,
                end: p.ends_at,
                location: p.location.clone(),
                attendees: p.attendees.clone(),
            })
            .map_err(|e| match e {
                known @ Error::UnknownOutcome(_) => known,
                other => Error::UnknownOutcome(format!(
                    "Google insert 未获得可靠回执，必须对账：{other}"
                )),
            })?;
        // Persist the insert receipt before independent readback. A failed
        // read must never turn an already applied insert into a retryable write.
        Ok(format!("event {} created ({})", created.id, created.title))
    }
}
