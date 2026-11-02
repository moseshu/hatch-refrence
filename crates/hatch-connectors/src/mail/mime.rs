// Copyright (c) 2026 Moses (https://github.com/moseshu)
// Author: Moses
// SPDX-License-Identifier: MIT

//! Minimal RFC 5322 / MIME composition for plain-text UTF-8 mail.

use super::OutgoingMail;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use hatch_core::{ids, time::Timestamp};

/// RFC 2047 encoded-word for non-ASCII header values.
pub fn encode_header(s: &str) -> String {
    if s.is_ascii() {
        s.to_string()
    } else {
        format!("=?UTF-8?B?{}?=", B64.encode(s.as_bytes()))
    }
}

fn wrap76(s: &str) -> String {
    s.as_bytes()
        .chunks(76)
        .map(|c| std::str::from_utf8(c).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\r\n")
}

/// Compose a message. Returns `(message_id, wire_text)` with CRLF line endings.
pub fn compose(m: &OutgoingMail, now: &Timestamp) -> (String, String) {
    let domain = m.from.rsplit_once('@').map(|(_, d)| d).unwrap_or("hatch.local");
    let seed = format!("{}{}{}", m.subject, m.body, now.timestamp_nanos_opt().unwrap_or(0));
    let message_id = format!("<{}@{domain}>", &ids::digest(&[&seed, &m.to.join(",")])[..24]);
    let mut h = vec![
        format!("From: {}", m.from),
        format!("To: {}", m.to.join(", ")),
        format!("Subject: {}", encode_header(&m.subject)),
        format!("Date: {}", now.to_rfc2822()),
        format!("Message-ID: {message_id}"),
    ];
    if let Some(r) = &m.in_reply_to {
        h.push(format!("In-Reply-To: {r}"));
    }
    if !m.references.is_empty() {
        h.push(format!("References: {}", m.references.join(" ")));
    }
    h.push("MIME-Version: 1.0".into());
    h.push("Content-Type: text/plain; charset=UTF-8".into());
    h.push("Content-Transfer-Encoding: base64".into());
    h.push("X-Mailer: hatch-refrence".into());
    let body = m.body.replace("\r\n", "\n").replace('\n', "\r\n");
    let text = format!("{}\r\n\r\n{}\r\n", h.join("\r\n"), wrap76(&B64.encode(body.as_bytes())));
    (message_id, text)
}

/// SMTP dot-stuffing (RFC 5321 §4.5.2).
pub fn dot_stuff(text: &str) -> String {
    text.split("\r\n")
        .map(|l| if l.starts_with('.') { format!(".{l}") } else { l.to_string() })
        .collect::<Vec<_>>()
        .join("\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composes_parseable_utf8_mail() {
        let m = OutgoingMail {
            from: "me@126.com".into(),
            to: vec!["boss@corp.com".into()],
            subject: "Re: 周报".into(),
            body: "王总，收到。\n明天上午交。".into(),
            in_reply_to: Some("<abc@corp.com>".into()),
            references: vec!["<abc@corp.com>".into()],
        };
        let now = hatch_core::time::parse_rfc3339("2026-11-03T06:00:00Z").unwrap();
        let (id, text) = compose(&m, &now);
        assert!(id.ends_with("@126.com>"));
        assert!(text.contains("Subject: =?UTF-8?B?"));
        assert!(text.contains("In-Reply-To: <abc@corp.com>"));
        let parsed = super::super::parse::parse_rfc822(text.as_bytes(), None).unwrap();
        assert_eq!(parsed.subject, "Re: 周报");
        assert!(parsed.body.contains("明天上午交"));
        assert_eq!(parsed.to, vec!["boss@corp.com".to_string()]);
    }

    #[test]
    fn dot_stuffing() {
        assert_eq!(dot_stuff("a\r\n.b\r\n..c"), "a\r\n..b\r\n...c");
        assert_eq!(encode_header("plain"), "plain");
    }
}
