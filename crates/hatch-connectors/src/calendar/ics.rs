//! Minimal iCalendar (RFC 5545) VEVENT reader.
//!
//! Handles line folding, UTC (`...Z`), `TZID=` local times, floating local
//! times (interpreted in the user's timezone), all-day `VALUE=DATE` events,
//! missing DTEND, and text unescaping. Recurrence rules are not expanded.

use super::Event;
use chrono::{Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use hatch_core::time::Timestamp;
use hatch_core::{Error, Result};

/// One content line: name, parameters, value.
type Prop = (String, Vec<(String, String)>, String);

fn unfold(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if (line.starts_with(' ') || line.starts_with('\t')) && !out.is_empty() {
            out.last_mut().unwrap().push_str(&line[1..]);
        } else {
            out.push(line.to_string());
        }
    }
    out
}

fn unescape(s: &str) -> String {
    s.replace("\\n", "\n").replace("\\N", "\n").replace("\\,", ",").replace("\\;", ";").replace("\\\\", "\\")
}

/// Split `NAME;P1=a;P2=b:VALUE` into name, params, value.
fn split_prop(line: &str) -> Option<Prop> {
    let colon = line.find(':')?;
    let (head, value) = (&line[..colon], &line[colon + 1..]);
    let mut parts = head.split(';');
    let name = parts.next()?.to_ascii_uppercase();
    let params = parts
        .filter_map(|p| p.split_once('=').map(|(k, v)| (k.to_ascii_uppercase(), v.trim_matches('"').to_string())))
        .collect();
    Some((name, params, value.to_string()))
}

/// Parse a DTSTART/DTEND value. Returns (instant, is_all_day).
fn parse_dt(value: &str, params: &[(String, String)], default_tz: &Tz) -> Result<(Timestamp, bool)> {
    let bad = || Error::Invalid(format!("bad iCalendar date-time {value:?}"));
    let is_date = params.iter().any(|(k, v)| k == "VALUE" && v == "DATE") || value.len() == 8;
    let tz: Tz = match params.iter().find(|(k, _)| k == "TZID") {
        Some((_, name)) => name.parse().unwrap_or(*default_tz),
        None => *default_tz,
    };
    if is_date {
        let d = NaiveDate::parse_from_str(value, "%Y%m%d").map_err(|_| bad())?;
        let local = tz.from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap()).earliest().ok_or_else(bad)?;
        return Ok((local.with_timezone(&Utc), true));
    }
    if let Some(v) = value.strip_suffix('Z') {
        let n = NaiveDateTime::parse_from_str(v, "%Y%m%dT%H%M%S").map_err(|_| bad())?;
        return Ok((Utc.from_utc_datetime(&n), false));
    }
    let n = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S").map_err(|_| bad())?;
    let local = tz.from_local_datetime(&n).earliest().ok_or_else(bad)?;
    Ok((local.with_timezone(&Utc), false))
}

/// Serialize one event as a minimal VEVENT (CalDAV PUT body). All times
/// are UTC (`...Z`); the local timezone is not needed because the event
/// is already stored in UTC. Text values are escaped per RFC 5545.
pub fn to_ics(e: &Event) -> String {
    fn esc(s: &str) -> String {
        s.replace('\\', "\\\\").replace('\n', "\\n").replace(',', "\\,").replace(';', "\\;")
    }
    fn fmt(ts: &Timestamp) -> String {
        ts.format("%Y%m%dT%H%M%SZ").to_string()
    }
    let stamp = fmt(&hatch_core::time::now());
    let mut out = String::from("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//hatch//CalDAV//EN\r\nBEGIN:VEVENT\r\n");
    out.push_str(&format!("UID:{}\r\n", e.uid));
    out.push_str(&format!("DTSTAMP:{stamp}\r\n"));
    out.push_str(&format!("DTSTART:{}\r\n", fmt(&e.starts_at)));
    out.push_str(&format!("DTEND:{}\r\n", fmt(&e.ends_at)));
    out.push_str(&format!("SUMMARY:{}\r\n", esc(&e.title)));
    if !e.location.is_empty() {
        out.push_str(&format!("LOCATION:{}\r\n", esc(&e.location)));
    }
    out.push_str("END:VEVENT\r\nEND:VCALENDAR\r\n");
    out
}
pub fn parse_ics(text: &str, default_tz: &Tz, source: &str) -> Result<Vec<Event>> {
    let mut events = Vec::new();
    let mut cur: Option<Vec<Prop>> = None;
    for line in unfold(text) {
        let upper = line.trim().to_ascii_uppercase();
        if upper == "BEGIN:VEVENT" {
            cur = Some(Vec::new());
            continue;
        }
        if upper == "END:VEVENT" {
            if let Some(props) = cur.take() {
                events.push(build_event(&props, default_tz, source)?);
            }
            continue;
        }
        if let (Some(props), Some(p)) = (cur.as_mut(), split_prop(&line)) {
            props.push(p);
        }
    }
    Ok(events)
}

fn build_event(props: &[Prop], tz: &Tz, source: &str) -> Result<Event> {
    let get = |n: &str| props.iter().find(|(k, _, _)| k == n);
    let (_, sp, sv) = get("DTSTART").ok_or_else(|| Error::Invalid("VEVENT without DTSTART".into()))?;
    let (start, all_day) = parse_dt(sv, sp, tz)?;
    let end = match get("DTEND") {
        Some((_, ep, ev)) => parse_dt(ev, ep, tz)?.0,
        None if all_day => start + Duration::days(1),
        None => start + Duration::hours(1),
    };
    let summary = get("SUMMARY").map(|(_, _, v)| unescape(v)).unwrap_or_else(|| "(无标题)".into());
    let uid = get("UID")
        .map(|(_, _, v)| v.clone())
        .unwrap_or_else(|| hatch_core::ids::stable_id("evt", &[&summary, &start.to_rfc3339()]));
    Ok(Event {
        uid,
        title: summary,
        starts_at: start,
        ends_at: end.max(start + Duration::minutes(1)),
        location: get("LOCATION").map(|(_, _, v)| unescape(v)).unwrap_or_default(),
        source: source.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatch_core::time::to_rfc3339;

    const ICS: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n\
BEGIN:VEVENT\r\nUID:u1\r\nSUMMARY:部门周会\\, 第 45 周\r\nDTSTART;TZID=Asia/Shanghai:20261104T100000\r\nDTEND;TZID=Asia/Shanghai:20261104T110000\r\nLOCATION:3号会议\r\n 室\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:u2\r\nSUMMARY:Call\r\nDTSTART:20261104T060000Z\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:u3\r\nSUMMARY:年假\r\nDTSTART;VALUE=DATE:20261106\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nSUMMARY:floating\r\nDTSTART:20261105T090000\r\nDTEND:20261105T093000\r\nEND:VEVENT\r\n\
END:VCALENDAR\r\n";

    #[test]
    fn parses_all_forms() {
        let tz: Tz = "Asia/Shanghai".parse().unwrap();
        let ev = parse_ics(ICS, &tz, "work.ics").unwrap();
        assert_eq!(ev.len(), 4);
        assert_eq!(ev[0].title, "部门周会, 第 45 周");
        assert_eq!(ev[0].location, "3号会议室");
        assert_eq!(to_rfc3339(&ev[0].starts_at), "2026-11-04T02:00:00Z");
        assert_eq!(to_rfc3339(&ev[1].ends_at), "2026-11-04T07:00:00Z");
        assert_eq!(to_rfc3339(&ev[2].starts_at), "2026-11-05T16:00:00Z");
        assert_eq!(to_rfc3339(&ev[2].ends_at), "2026-11-06T16:00:00Z");
        assert_eq!(to_rfc3339(&ev[3].starts_at), "2026-11-05T01:00:00Z");
        assert!(ev[3].uid.starts_with("evt_"));
        assert!(parse_ics("BEGIN:VEVENT\nSUMMARY:x\nEND:VEVENT", &tz, "s").is_err());
    }
}
