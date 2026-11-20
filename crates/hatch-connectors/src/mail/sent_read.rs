//! Strict readback of a provider-owned sent message.
use super::ParsedMail;
use base64::Engine;
use hatch_core::{Error, Result};

pub(super) fn parse_gmail_sent(text: &str, id: &str) -> Result<ParsedMail> {
    let v: serde_json::Value = serde_json::from_str(text)?;
    if v["id"].as_str() != Some(id)
        || !v["labelIds"]
            .as_array()
            .is_some_and(|a| a.iter().any(|l| l.as_str() == Some("SENT")))
    {
        return Err(Error::Connector("Gmail 回读不是指定的已发送消息".into()));
    }
    let raw = v["raw"]
        .as_str()
        .ok_or_else(|| Error::Connector("Gmail 回读缺少 raw 内容".into()))?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(raw.trim_end_matches('='))
        .map_err(|_| Error::Connector("Gmail raw 编码无效".into()))?;
    let mut mail = super::parse::parse_rfc822(&bytes, None)?;
    mail.body = strip_helper_marker(&mail.body).to_string();
    Ok(mail)
}

/// The vendored helper rewrites `format=raw` responses, prefixing the
/// decoded body with a `--- decoded <type> part ---` line. That line
/// is helper display chrome, not message content: strip exactly one
/// such leading line so readback digests compare the real body.
fn strip_helper_marker(body: &str) -> &str {
    let Some((first, rest)) = body.split_once('\n') else {
        return body;
    };
    let first = first.trim_end_matches('\r');
    if first.starts_with("--- decoded ") && first.ends_with(" part ---") {
        rest
    } else {
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strips_helper_marker_line_from_body() {
        use base64::Engine;
        let raw = b"From: me@example.com\r\nTo: you@example.com\r\nSubject: hi\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n--- decoded text/plain part ---\r\nhello body";
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
        let text = format!(r#"{{"id":"m1","labelIds":["SENT"],"raw":"{encoded}"}}"#);
        let mail = parse_gmail_sent(&text, "m1").unwrap();
        assert_eq!(mail.body.trim_end(), "hello body");
        // A body without the marker is untouched.
        let raw2 = b"From: me@example.com\r\nTo: you@example.com\r\nSubject: hi\r\n\r\nplain body";
        let encoded2 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw2);
        let text2 = format!(r#"{{"id":"m2","labelIds":["SENT"],"raw":"{encoded2}"}}"#);
        let mail2 = parse_gmail_sent(&text2, "m2").unwrap();
        assert_eq!(mail2.body.trim_end(), "plain body");
    }

    #[test]
    fn refuses_wrong_id_or_unsent_message() {
        assert!(
            parse_gmail_sent(r#"{"id":"other","labelIds":["SENT"],"raw":""}"#, "wanted").is_err()
        );
        assert!(
            parse_gmail_sent(r#"{"id":"wanted","labelIds":["DRAFT"],"raw":""}"#, "wanted").is_err()
        );
    }
}
