//! Per-connector token records.
//!
//! The on-disk store (`tokens.json`, 0600) maps a token name to a record.
//! The legacy vault-lite format mapped a name straight to a string value;
//! that is still read (migrated in memory to a record with no metadata)
//! and rewritten in the new format on the next store.
//!
//! A record never leaves the vault helper process except through the
//! helper's own operations (`inject` spawns a child with the value,
//! `request` performs an HTTP call with it, `reveal` hands it to the
//! parent only for the spawn instant — see `client.rs`).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One stored token.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TokenRecord {
    /// The secret value. Only the helper process holds this in memory.
    pub value: String,
    /// Which connector / purpose this token belongs to, if recorded.
    pub connector: Option<String>,
    /// The account this token was issued for (e.g. the mailbox it may
    /// act as), if recorded. Refresh and revoke check it.
    pub account: Option<String>,
    /// Space-separated scopes granted with this token, if recorded.
    /// A refresh response may narrow them, never widen them.
    pub scope: Option<String>,
    /// Token/refresh endpoint recorded when the token was stored (the
    /// full URL). Refresh must talk to this provider's host: a stored
    /// refresh URL pointing anywhere else is refused, so a tampered
    /// tokens file cannot redirect a refresh to an attacker's host.
    pub provider_url: Option<String>,
    /// Revocation endpoint, if the provider has one. `None` means
    /// revoke is local-only (the token is still blocked everywhere
    /// in hatch, the provider is simply not told).
    pub revoke_url: Option<String>,
    /// Rotation generation, bumped on every successful refresh or
    /// revoke. Lets a refresh detect that another helper instance
    /// rotated the token first and discard its own fresh tokens
    /// instead of clobbering them.
    pub generation: u64,
    /// True once revoked: the token is blocked everywhere in hatch
    /// (helper ops and the legacy file path all fail closed) until a
    /// fresh authorization stores a new record.
    pub revoked: bool,
    /// RFC 3339 time the token was revoked, if revoked.
    pub revoked_at: Option<String>,
    /// Unix timestamp (seconds) after which the token is expired, if known.
    pub expires_at: Option<i64>,
    /// Refresh token, held by the helper only. `None` means no refresh
    /// token is stored for this record.
    pub refresh_token: Option<String>,
    /// Whether a refresh flow is supported for this connector at all.
    /// The refresh flow itself is a stub (see `helper.rs`): it reports
    /// "not supported" unless a connector implements it in tests.
    pub refresh_supported: bool,
    /// Token endpoint used for refresh (mock connectors in tests only).
    pub refresh_url: Option<String>,
}

impl TokenRecord {
    pub fn new(value: &str) -> Self {
        TokenRecord { value: value.to_string(), ..Default::default() }
    }

    /// True when `expires_at` is set and in the past.
    pub fn is_expired(&self, now_unix: i64) -> bool {
        self.expires_at.map(|exp| now_unix >= exp).unwrap_or(false)
    }

    /// Metadata for `list`: names and state only, never values.
    pub fn summary(&self, name: &str, now_unix: i64) -> TokenSummary {
        TokenSummary {
            name: name.to_string(),
            connector: self.connector.clone(),
            account: self.account.clone(),
            scope: self.scope.clone(),
            expires_at: self.expires_at,
            expired: self.is_expired(now_unix),
            has_refresh_token: self.refresh_token.is_some(),
            refresh_supported: self.refresh_supported,
            revoked: self.revoked,
            revoked_at: self.revoked_at.clone(),
        }
    }
}

/// What `list` returns for one token: metadata only, never the value.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TokenSummary {
    pub name: String,
    pub connector: Option<String>,
    /// The account the token was issued for, if recorded.
    #[serde(default)]
    pub account: Option<String>,
    /// The scopes granted with the token, if recorded.
    #[serde(default)]
    pub scope: Option<String>,
    pub expires_at: Option<i64>,
    pub expired: bool,
    pub has_refresh_token: bool,
    pub refresh_supported: bool,
    /// True once revoked (see [`TokenRecord::revoked`]).
    #[serde(default)]
    pub revoked: bool,
    /// RFC 3339 time the token was revoked, if revoked.
    #[serde(default)]
    pub revoked_at: Option<String>,
}

/// Parse the on-disk store: either the new `{name: record}` format or the
/// legacy `{name: "value"}` format. A record value that is a bare string
/// migrates to [`TokenRecord::new`]. Anything else is an error; the file
/// is never silently wiped.
pub fn parse_store(text: &str) -> Result<BTreeMap<String, TokenRecord>, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|_| "corrupt tokens file (invalid JSON)".to_string())?;
    let object = value
        .as_object()
        .ok_or_else(|| "corrupt tokens file (not a JSON object)".to_string())?;
    let mut map = BTreeMap::new();
    for (key, val) in object {
        if let Some(s) = val.as_str() {
            map.insert(key.clone(), TokenRecord::new(s));
        } else if val.is_object() {
            let rec: TokenRecord = serde_json::from_value(val.clone())
                .map_err(|_| format!("corrupt tokens file (bad record for `{key}`)"))?;
            map.insert(key.clone(), rec);
        } else {
            return Err(format!("corrupt tokens file (bad value for `{key}`)"));
        }
    }
    Ok(map)
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_legacy_string_format() {
        let map = parse_store(r#"{"a": "v1", "b": "v2"}"#).unwrap();
        assert_eq!(map["a"].value, "v1");
        assert_eq!(map["b"].connector, None);
    }

    #[test]
    fn parse_new_record_format() {
        let map = parse_store(
            r#"{"a": {"value": "v1", "connector": "mock", "expires_at": 9999999999}}"#,
        )
        .unwrap();
        assert_eq!(map["a"].connector.as_deref(), Some("mock"));
        assert!(!map["a"].is_expired(now_unix()));
    }

    #[test]
    fn parse_mixed_formats() {
        let map = parse_store(r#"{"a": "v1", "b": {"value": "v2"}}"#).unwrap();
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse_store("{nope").is_err());
        assert!(parse_store("[1,2]").is_err());
        assert!(parse_store(r#"{"a": 42}"#).is_err());
    }

    #[test]
    fn expiry_and_summary_hide_value() {
        let mut rec = TokenRecord::new("test-token-secret");
        rec.expires_at = Some(now_unix() - 10);
        assert!(rec.is_expired(now_unix()));
        let summary = rec.summary("a", now_unix());
        assert!(summary.expired);
        let text = serde_json::to_string(&summary).unwrap();
        assert!(!text.contains("test-token-secret"));
    }
}
