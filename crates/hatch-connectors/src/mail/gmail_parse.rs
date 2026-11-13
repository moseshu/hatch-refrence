//! Parsing the Gmail helper's JSON output (`+triage` / `+read` /
//! `+send`). Shapes are parsed tolerantly (several key spellings
//! accepted) because the helper's JSON is only documented by example
//! (`jq '.[].subject'`, `jq '.body_text'`).

use super::ParsedMail;
use chrono::{DateTime, Utc};
use hatch_core::time::{parse_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use serde_json::Value;

pub(crate) fn get_str<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    for k in keys {
        if let Some(s) = v.get(*k).and_then(Value::as_str) {
            return Some(s);
        }
    }
    None
}

pub(crate) fn get_string(v: &Value, keys: &[&str]) -> Option<String> {
    get_str(v, keys).map(str::to_string)
}

/// Split `Name <addr>`, a bare address, or a bare name.
pub(crate) fn parse_sender(raw: &str) -> (String, String) {
    let raw = raw.trim();
    if let Some((name, rest)) = raw.split_once('<') {
        let addr = rest.trim_end_matches('>').trim().to_string();
        (name.trim().to_string(), addr)
    } else if raw.contains('@') {
        (String::new(), raw.to_string())
    } else {
        (raw.to_string(), String::new())
    }
}

pub(crate) fn parse_sent_at(v: &Value) -> Option<Timestamp> {
    let s = match v {
        Value::String(s) => Some(s.as_str()),
        Value::Object(_) => get_str(v, &["user_local", "rfc3339", "value"]),
        _ => None,
    }?;
    parse_rfc3339(s).ok().or_else(|| {
        DateTime::parse_from_rfc2822(s)
            .ok()
            .map(|d| d.with_timezone(&Utc))
    })
}

pub(crate) struct TriageItem {
    pub(crate) id: String,
}

pub(crate) fn parse_triage_list(text: &str) -> Result<Vec<TriageItem>> {
    let v: Value = serde_json::from_str(text)
        .map_err(|e| Error::Connector(format!("解析 +triage 输出失败：{e}")))?;
    let arr = v
        .as_array()
        .ok_or_else(|| Error::Connector("+triage 输出不是 JSON 数组".into()))?;
    let mut items = Vec::new();
    for it in arr {
        let id = get_string(it, &["id", "message_id", "messageId"])
            .ok_or_else(|| Error::Connector("+triage 输出缺少消息 id".into()))?;
        items.push(TriageItem { id });
    }
    Ok(items)
}

pub(crate) fn parse_read_message(text: &str, fallback_id: &str) -> Result<ParsedMail> {
    let v: Value = serde_json::from_str(text)
        .map_err(|e| Error::Connector(format!("解析 +read 输出失败：{e}")))?;
    // `--headers` may nest the headers under a `headers` object.
    let headers = v.get("headers").cloned().unwrap_or(Value::Null);
    let from_raw = get_string(&v, &["from", "sender"])
        .or_else(|| get_string(&headers, &["From", "from"]))
        .unwrap_or_default();
    let (from_name, from_addr) = parse_sender(&from_raw);
    let to = match v.get("to").or_else(|| headers.get("To")) {
        Some(Value::String(s)) => s.split(',').map(|a| a.trim().to_string()).collect(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => vec![],
    };
    let subject = get_string(&v, &["subject"])
        .or_else(|| get_string(&headers, &["Subject", "subject"]))
        .unwrap_or_default();
    let sent_at = v
        .get("date")
        .or_else(|| v.get("message_sent_at"))
        .or_else(|| headers.get("Date"))
        .and_then(parse_sent_at);
    let body = get_string(&v, &["body_text", "body"]).unwrap_or_default();
    let message_id = get_string(&v, &["id", "message_id", "messageId"])
        .unwrap_or_else(|| fallback_id.to_string());
    Ok(ParsedMail {
        uid: None,
        message_id,
        from_addr,
        from_name,
        to,
        subject,
        sent_at,
        body,
    })
}

pub(crate) fn parse_send_id(text: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        if let Some(id) = get_string(&v, &["id", "message_id", "messageId"]) {
            if !id.is_empty() {
                return id;
            }
        }
    }
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.chars().take(120).collect())
        .unwrap_or_else(|| "gmail-sent".to_string())
}

#[cfg(test)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sender_split() {
        assert_eq!(
            parse_sender("李婷 <li.ting@corp.example>"),
            ("李婷".to_string(), "li.ting@corp.example".to_string())
        );
        assert_eq!(
            parse_sender("li.ting@corp.example"),
            (String::new(), "li.ting@corp.example".to_string())
        );
        assert_eq!(parse_sender("李婷"), ("李婷".to_string(), String::new()));
    }

    #[test]
    fn read_parses_headers_object_and_rfc2822() {
        let text = serde_json::json!({
            "message_id": "m9",
            "headers": {"From": "王宇 <wang.yu@corp.example>", "To": "moseshu25@gmail.com", "Subject": "预算确认", "Date": "Tue, 03 Nov 2026 10:00:00 +0800"},
            "body_text": "预算上限 5 万元。"
        })
        .to_string();
        let m = parse_read_message(&text, "m9").unwrap();
        assert_eq!(m.from_name, "王宇");
        assert_eq!(m.to, vec!["moseshu25@gmail.com"]);
        assert!(m.sent_at.is_some());
        assert_eq!(m.body, "预算上限 5 万元。");
    }
}
