//! Real Google Calendar writes through the `hatch_gws_cli` helper
//! (`calendar events insert` / `events get` / `events list`).
//!
//! Same shape as the Gmail module: the helper owns the credential
//! (the connector's OAuth grant); this module never sees a key, a
//! token or a password — it only passes argv to the helper binary
//! and parses its JSON output. The helper path comes from
//! `HATCH_GWS_CLI` (see `crate::gws_cli`).
//!
//! Writes go to the `primary` calendar. An event with attendees
//! notifies them (`sendUpdates: all`); every insert is followed by
//! an `events get` that reads the event back, and the Google event
//! id is the receipt — never a locally generated id.

use super::google_parse::{parse_event, parse_event_list};
use crate::gws_cli::{CliRunner, ProcessRunner};
use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// The calendar every write targets (the user's primary calendar).
pub const PRIMARY_CALENDAR_ID: &str = "primary";
/// A single helper call may run this long before it is killed.
const INSERT_TIMEOUT: Duration = Duration::from_secs(120);
const GET_TIMEOUT: Duration = Duration::from_secs(60);
const LIST_TIMEOUT: Duration = Duration::from_secs(60);

/// A new event to create on Google Calendar. Times are UTC; the
/// helper sends them as RFC3339 `dateTime` values.
#[derive(Clone, Debug, PartialEq)]
pub struct NewGoogleEvent {
    pub title: String,
    pub start: Timestamp,
    pub end: Timestamp,
    pub location: String,
    pub attendees: Vec<String>,
}

/// An event as Google returned it. `id` is the receipt: the caller
/// records it, never a locally generated id.
#[derive(Clone, Debug, PartialEq)]
pub struct GoogleEvent {
    pub id: String,
    pub title: String,
    pub start: Timestamp,
    pub end: Timestamp,
    pub location: String,
    pub attendees: Vec<String>,
}

impl GoogleEvent {
    pub fn overlaps(&self, start: &Timestamp, end: &Timestamp) -> bool {
        self.start < *end && self.end > *start
    }
}

/// Where Google Calendar writes go. Returns the event as Google
/// stored it.
pub trait GoogleCalendarWriter: Send + Sync {
    /// Create the event; guests are notified when any are listed.
    fn insert(&self, event: &NewGoogleEvent) -> Result<GoogleEvent>;
    /// Read one event back by its Google id.
    fn get(&self, event_id: &str) -> Result<GoogleEvent>;
    /// Events overlapping `[start, end)` on the primary calendar.
    fn list(&self, start: Timestamp, end: Timestamp) -> Result<Vec<GoogleEvent>>;
}

/// The real writer: `calendar events insert` / `events get` /
/// `events list` through the helper binary.
pub struct GoogleCalendar {
    runner: Arc<dyn CliRunner>,
}

impl GoogleCalendar {
    pub fn new(cli: PathBuf) -> Self {
        GoogleCalendar {
            runner: Arc::new(ProcessRunner::new(cli)),
        }
    }

    pub fn with_runner(runner: Arc<dyn CliRunner>) -> Self {
        GoogleCalendar { runner }
    }
}

impl GoogleCalendarWriter for GoogleCalendar {
    fn insert(&self, event: &NewGoogleEvent) -> Result<GoogleEvent> {
        if event.end <= event.start {
            return Err(Error::Invalid("event ends before it starts".into()));
        }
        let mut body = serde_json::json!({
            "summary": event.title,
            "start": {"dateTime": to_rfc3339(&event.start)},
            "end": {"dateTime": to_rfc3339(&event.end)},
        });
        if !event.location.is_empty() {
            body["location"] = event.location.clone().into();
        }
        if !event.attendees.is_empty() {
            body["attendees"] = event
                .attendees
                .iter()
                .map(|a| serde_json::json!({"email": a}))
                .collect();
        }
        let argv: Vec<String> = vec![
            "calendar".into(),
            "events".into(),
            "insert".into(),
            "--params".into(),
            params_json(PRIMARY_CALENDAR_ID, !event.attendees.is_empty()),
            "--json".into(),
            body.to_string(),
        ];
        let out = self.runner.run(&argv, INSERT_TIMEOUT)?;
        parse_event(&out)
    }

    fn get(&self, event_id: &str) -> Result<GoogleEvent> {
        let argv: Vec<String> = vec![
            "calendar".into(),
            "events".into(),
            "get".into(),
            "--params".into(),
            serde_json::json!({"calendarId": PRIMARY_CALENDAR_ID, "eventId": event_id}).to_string(),
        ];
        let out = self.runner.run(&argv, GET_TIMEOUT)?;
        parse_event(&out)
    }

    fn list(&self, start: Timestamp, end: Timestamp) -> Result<Vec<GoogleEvent>> {
        let argv: Vec<String> = vec![
            "calendar".into(),
            "events".into(),
            "list".into(),
            "--params".into(),
            serde_json::json!({
                "calendarId": PRIMARY_CALENDAR_ID,
                "timeMin": to_rfc3339(&start),
                "timeMax": to_rfc3339(&end),
                "singleEvents": true,
                "orderBy": "startTime",
            })
            .to_string(),
        ];
        let out = self.runner.run(&argv, LIST_TIMEOUT)?;
        parse_event_list(&out)
    }
}

fn params_json(calendar_id: &str, notify: bool) -> String {
    if notify {
        serde_json::json!({"calendarId": calendar_id, "sendUpdates": "all"}).to_string()
    } else {
        serde_json::json!({"calendarId": calendar_id}).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatch_core::time::parse_rfc3339;
    use serde_json::Value;
    use std::sync::Mutex;

    /// Canned runner: answers by substring match on the joined argv,
    /// and records every call for assertions.
    struct CannedRunner {
        responses: Vec<(String, String)>,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl CannedRunner {
        fn new(responses: Vec<(String, String)>) -> Self {
            CannedRunner {
                responses,
                calls: Mutex::new(vec![]),
            }
        }

        /// The JSON value of the flag's argument in the recorded call.
        fn flag_json(&self, call: usize, flag: &str) -> Value {
            let calls = self.calls.lock().unwrap();
            let argv = &calls[call];
            let i = argv.iter().position(|a| a == flag).unwrap();
            serde_json::from_str(&argv[i + 1]).unwrap()
        }
    }

    impl CliRunner for CannedRunner {
        fn run(&self, argv: &[String], _timeout: Duration) -> Result<String> {
            self.calls.lock().unwrap().push(argv.to_vec());
            let joined = argv.join(" ");
            for (pat, resp) in &self.responses {
                if joined.contains(pat.as_str()) {
                    return Ok(resp.clone());
                }
            }
            Err(Error::Connector(format!(
                "canned runner: no response for {joined}"
            )))
        }
    }

    fn event_json() -> String {
        serde_json::json!({
            "id": "google-ev-1",
            "summary": "项目评审会",
            "start": {"dateTime": "2026-11-04T05:00:00Z"},
            "end": {"dateTime": "2026-11-04T06:00:00Z"},
            "location": "3 号会议室",
            "attendees": [{"email": "li.ting@corp.example"}]
        })
        .to_string()
    }

    fn new_event() -> NewGoogleEvent {
        NewGoogleEvent {
            title: "项目评审会".into(),
            start: parse_rfc3339("2026-11-04T05:00:00Z").unwrap(),
            end: parse_rfc3339("2026-11-04T06:00:00Z").unwrap(),
            location: "3 号会议室".into(),
            attendees: vec!["li.ting@corp.example".into()],
        }
    }

    #[test]
    fn insert_with_attendees_notifies_and_returns_the_google_id() {
        let runner = Arc::new(CannedRunner::new(vec![(
            "events insert".into(),
            event_json(),
        )]));
        let cal = GoogleCalendar::with_runner(runner.clone());
        let ev = cal.insert(&new_event()).unwrap();
        assert_eq!(ev.id, "google-ev-1");
        assert_eq!(ev.title, "项目评审会");
        assert_eq!(ev.attendees, vec!["li.ting@corp.example".to_string()]);
        assert_eq!(ev.location, "3 号会议室");
        let params = runner.flag_json(0, "--params");
        assert_eq!(params["calendarId"], "primary");
        assert_eq!(params["sendUpdates"], "all");
        let body = runner.flag_json(0, "--json");
        assert_eq!(body["summary"], "项目评审会");
        assert_eq!(body["start"]["dateTime"], "2026-11-04T05:00:00Z");
        assert_eq!(body["attendees"][0]["email"], "li.ting@corp.example");
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls[0][0], "calendar");
        assert_eq!(calls[0][1], "events");
        assert_eq!(calls[0][2], "insert");
    }

    #[test]
    fn insert_without_attendees_omits_send_updates() {
        let runner = Arc::new(CannedRunner::new(vec![(
            "events insert".into(),
            event_json(),
        )]));
        let cal = GoogleCalendar::with_runner(runner.clone());
        let mut ev = new_event();
        ev.attendees = vec![];
        cal.insert(&ev).unwrap();
        let params = runner.flag_json(0, "--params");
        assert_eq!(params["calendarId"], "primary");
        assert!(params.get("sendUpdates").is_none(), "无嘉宾时不得通知");
        let body = runner.flag_json(0, "--json");
        assert!(body.get("attendees").is_none(), "无嘉宾时不发 attendees");
    }

    #[test]
    fn get_and_list_read_back() {
        let runner = Arc::new(CannedRunner::new(vec![
            ("events get".into(), event_json()),
            (
                "events list".into(),
                serde_json::json!({"items": [serde_json::from_str::<Value>(&event_json()).unwrap()]}).to_string(),
            ),
        ]));
        let cal = GoogleCalendar::with_runner(runner.clone());
        let ev = cal.get("google-ev-1").unwrap();
        assert_eq!(ev.id, "google-ev-1");
        let params = runner.flag_json(0, "--params");
        assert_eq!(params["eventId"], "google-ev-1");
        let list = cal
            .list(
                parse_rfc3339("2026-11-04T04:00:00Z").unwrap(),
                parse_rfc3339("2026-11-04T07:00:00Z").unwrap(),
            )
            .unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "google-ev-1");
        let params = runner.flag_json(1, "--params");
        assert_eq!(params["singleEvents"], true);
        assert_eq!(params["timeMin"], "2026-11-04T04:00:00Z");
        assert!(list[0].overlaps(
            &parse_rfc3339("2026-11-04T05:30:00Z").unwrap(),
            &parse_rfc3339("2026-11-04T06:30:00Z").unwrap()
        ));
    }

    #[test]
    fn bad_json_and_missing_id_are_connector_errors() {
        let runner = Arc::new(CannedRunner::new(vec![
            ("events get".into(), "not json at all".into()),
            (
                "events list".into(),
                serde_json::json!({"items": [{"summary": "no id"}]}).to_string(),
            ),
        ]));
        let cal = GoogleCalendar::with_runner(runner.clone());
        assert!(cal.get("x").is_err(), "非 JSON 应报错");
        assert!(
            cal.list(
                parse_rfc3339("2026-11-04T04:00:00Z").unwrap(),
                parse_rfc3339("2026-11-04T07:00:00Z").unwrap()
            )
            .is_err(),
            "没有 id 的事件应报错"
        );
    }

    #[test]
    fn end_before_start_is_refused_before_any_call() {
        let runner = Arc::new(CannedRunner::new(vec![]));
        let cal = GoogleCalendar::with_runner(runner.clone());
        let mut ev = new_event();
        ev.end = ev.start;
        assert!(cal.insert(&ev).is_err());
        assert!(
            runner.calls.lock().unwrap().is_empty(),
            "拒绝前不得调 helper"
        );
    }
}
