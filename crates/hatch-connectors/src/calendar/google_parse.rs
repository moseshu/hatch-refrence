//! Parsing the Google Calendar helper's JSON output (`events
//! insert` / `events get` / `events list`). Tolerant on optional
//! fields, but a missing `id` is a hard error — without it there
//! is no receipt.

use super::google::GoogleEvent;
use hatch_core::time::{parse_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use serde_json::Value;

/// Parse one event resource: `{"id", "summary", "start":
/// {"dateTime"|"date"}, ...}`. Tolerant on optional fields, but a
/// missing `id` is a hard error — without it there is no receipt.
pub(crate) fn parse_event(out: &str) -> Result<GoogleEvent> {
    let v: Value = serde_json::from_str(out)
        .map_err(|e| Error::Connector(format!("Google 日历返回的不是 JSON：{e}")))?;
    parse_event_value(&v)
}

pub(crate) fn parse_event_value(v: &Value) -> Result<GoogleEvent> {
    if v.get("status").and_then(Value::as_str) == Some("cancelled") {
        return Err(Error::Connector("Google 日历事件已取消，不能作为有效交付证据".into()));
    }
    let id = v
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Connector("Google 日历返回的事件没有 id".into()))?;
    Ok(GoogleEvent {
        id: id.to_string(),
        title: v
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        start: parse_when(v.get("start"))?,
        end: parse_when(v.get("end"))?,
        location: v
            .get("location")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        attendees: parse_attendees(v),
    })
}

fn parse_when(w: Option<&Value>) -> Result<Timestamp> {
    let Some(w) = w else {
        return Err(Error::Connector(
            "Google 日历返回的事件没有开始/结束时间".into(),
        ));
    };
    if let Some(dt) = w.get("dateTime").and_then(Value::as_str) {
        return parse_rfc3339(dt)
            .map_err(|e| Error::Connector(format!("Google 日历返回的时间解析失败：{e}")));
    }
    if let Some(d) = w.get("date").and_then(Value::as_str) {
        // All-day: the day at midnight UTC. The end stays exclusive,
        // so overlap checks still work.
        let day = chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d")
            .map_err(|e| Error::Connector(format!("Google 日历返回的日期解析失败：{e}")))?;
        return Ok(day.and_hms_opt(0, 0, 0).unwrap().and_utc());
    }
    Err(Error::Connector(
        "Google 日历返回的时间既不是 dateTime 也不是 date".into(),
    ))
}

fn parse_attendees(v: &Value) -> Vec<String> {
    v.get("attendees")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.get("email").and_then(Value::as_str))
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn parse_event_list(out: &str) -> Result<Vec<GoogleEvent>> {
    let v: Value = serde_json::from_str(out)
        .map_err(|e| Error::Connector(format!("Google 日历返回的不是 JSON：{e}")))?;
    let items = v
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Connector("Google 日历返回的列表没有 items".into()))?;
    items.iter().map(parse_event_value).collect()
}
