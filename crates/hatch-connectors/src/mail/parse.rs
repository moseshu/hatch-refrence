// Copyright (c) 2026 Moses (https://github.com/moseshu)
// Author: Moses
// SPDX-License-Identifier: MIT

//! RFC 822 parsing (charset-aware, including GBK/GB18030 used by Chinese mail).

use super::ParsedMail;
use chrono::{TimeZone, Utc};
use hatch_core::{ids, Error, Result};
use mail_parser::MessageParser;

const MAX_BODY_CHARS: usize = 20_000;

pub fn parse_rfc822(raw: &[u8], uid: Option<u32>) -> Result<ParsedMail> {
    let msg = MessageParser::default()
        .parse(raw)
        .ok_or_else(|| Error::Connector("unparsable message".into()))?;
    let (from_addr, from_name) = msg
        .from()
        .and_then(|a| a.first())
        .map(|a| {
            (
                a.address().unwrap_or_default().to_ascii_lowercase(),
                a.name().unwrap_or_default().to_string(),
            )
        })
        .unwrap_or_default();
    let to = msg
        .to()
        .map(|a| a.iter().filter_map(|x| x.address().map(|s| s.to_ascii_lowercase())).collect())
        .unwrap_or_default();
    let message_id = match msg.message_id() {
        Some(id) if !id.is_empty() => format!("<{}>", id.trim_matches(|c| c == '<' || c == '>')),
        _ => format!("<{}@hatch.generated>", &ids::digest(&[&String::from_utf8_lossy(raw)])[..24]),
    };
    let sent_at = msg.date().and_then(|d| Utc.timestamp_opt(d.to_timestamp(), 0).single());
    let mut body: String = msg.body_text(0).map(|b| b.into_owned()).unwrap_or_default();
    if body.chars().count() > MAX_BODY_CHARS {
        body = body.chars().take(MAX_BODY_CHARS).collect::<String>() + "\n[……正文过长已截断]";
    }
    Ok(ParsedMail {
        uid,
        message_id,
        from_addr,
        from_name,
        to,
        subject: msg.subject().unwrap_or_default().to_string(),
        sent_at,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gbk_encoded_subject_and_body() {
        // "会议通知" in GBK, base64 encoded-word; body in GBK quoted-printable.
        let raw = b"From: =?GBK?B?1cXI/Q==?= <ZhangSan@Corp.com>\r\n\
To: me@126.com\r\n\
Subject: =?GBK?B?u+HS6c2o1qo=?=\r\n\
Date: Tue, 3 Nov 2026 09:00:00 +0800\r\n\
Message-ID: <m1@corp.com>\r\n\
Content-Type: text/plain; charset=GBK\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\
\r\n\
=C3=F7=CC=EC=BF=AA=BB=E1\r\n";
        let m = parse_rfc822(raw, Some(7)).unwrap();
        assert_eq!(m.subject, "会议通知");
        assert_eq!(m.from_name, "张三");
        assert_eq!(m.from_addr, "zhangsan@corp.com");
        assert_eq!(m.message_id, "<m1@corp.com>");
        assert!(m.body.contains("明天开会"), "{}", m.body);
        assert_eq!(m.uid, Some(7));
        assert_eq!(hatch_core::time::to_rfc3339(&m.sent_at.unwrap()), "2026-11-03T01:00:00Z");
    }

    #[test]
    fn synthesises_missing_message_id() {
        let m = parse_rfc822(b"From: a@b.c\r\nSubject: x\r\n\r\nhi\r\n", None).unwrap();
        assert!(m.message_id.ends_with("@hatch.generated>"));
    }
}
