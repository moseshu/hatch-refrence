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
    super::parse::parse_rfc822(&bytes, None)
}

#[cfg(test)]
mod tests {
    use super::*;
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
