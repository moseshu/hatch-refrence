//! Secret detection and redaction.
//!
//! Credentials, one-time codes, card numbers and national id numbers must never
//! enter memory, notifications, summaries or logs. This module is the single
//! place that decides what "looks like a secret". It is deliberately
//! conservative: card and id numbers are checksum-validated so ordinary order
//! numbers do not trigger false positives.
//!
//! Note on CJK: `\b` does not fire between a Han character and a digit (both
//! are word characters in Unicode), so numeric patterns use explicit
//! `(?:^|[^0-9])` boundaries instead.

use regex::Regex;
use serde::Serialize;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretKind {
    ApiKey,
    PrivateKey,
    BankCard,
    NationalId,
    Password,
    OneTimeCode,
}

impl SecretKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SecretKind::ApiKey => "api_key",
            SecretKind::PrivateKey => "private_key",
            SecretKind::BankCard => "bank_card",
            SecretKind::NationalId => "national_id",
            SecretKind::Password => "password",
            SecretKind::OneTimeCode => "one_time_code",
        }
    }
}

/// A detected secret: kind and byte range of the sensitive part.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub kind: SecretKind,
    pub start: usize,
    pub end: usize,
}

struct Patterns {
    api_key: Regex,
    private_key: Regex,
    card: Regex,
    id: Regex,
    password: Regex,
    otp: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| Patterns {
        api_key: Regex::new(
            r"(sk-[A-Za-z0-9_\-]{16,}|ghp_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|AKIA[0-9A-Z]{16}|xox[baprs]-[A-Za-z0-9\-]{10,}|AIza[0-9A-Za-z_\-]{30,})",
        )
        .unwrap(),
        private_key: Regex::new(r"-----BEGIN [A-Z ]*PRIVATE KEY-----").unwrap(),
        card: Regex::new(r"(?:^|[^0-9])([0-9]{13,19})(?:$|[^0-9])").unwrap(),
        id: Regex::new(r"(?:^|[^0-9])([0-9]{17}[0-9Xx])(?:$|[^0-9A-Za-z])").unwrap(),
        password: Regex::new(
            r"(?i)(?:password|passwd|pwd|密码|口令|授权码)\s*(?:[:：=]|是|为)\s*(\S{3,})",
        )
        .unwrap(),
        otp: Regex::new(
            r"(?i)(?:verification code|one-time code|security code|otp|验证码|校验码|动态码|动态密码)[^0-9]{0,12}([0-9]{4,8})(?:$|[^0-9])",
        )
        .unwrap(),
    })
}

/// Luhn checksum used by payment cards.
pub fn luhn_valid(digits: &str) -> bool {
    let mut sum = 0u32;
    for (i, c) in digits.chars().rev().enumerate() {
        let Some(mut d) = c.to_digit(10) else { return false };
        if i % 2 == 1 {
            d *= 2;
            if d > 9 {
                d -= 9;
            }
        }
        sum += d;
    }
    !digits.is_empty() && sum.is_multiple_of(10)
}

/// GB 11643 checksum for 18-digit resident identity numbers.
pub fn national_id_valid(id: &str) -> bool {
    const W: [u32; 17] = [7, 9, 10, 5, 8, 4, 2, 1, 6, 3, 7, 9, 10, 5, 8, 4, 2];
    const CHECK: [char; 11] = ['1', '0', 'X', '9', '8', '7', '6', '5', '4', '3', '2'];
    let chars: Vec<char> = id.chars().collect();
    if chars.len() != 18 {
        return false;
    }
    let mut sum = 0;
    for i in 0..17 {
        let Some(d) = chars[i].to_digit(10) else { return false };
        sum += d * W[i];
    }
    chars[17].to_ascii_uppercase() == CHECK[(sum % 11) as usize]
}

/// Find every secret in `text`, sorted by position, overlaps removed.
pub fn detect(text: &str) -> Vec<Finding> {
    let p = patterns();
    let mut out = Vec::new();
    for m in p.api_key.find_iter(text) {
        out.push(Finding { kind: SecretKind::ApiKey, start: m.start(), end: m.end() });
    }
    for m in p.private_key.find_iter(text) {
        out.push(Finding { kind: SecretKind::PrivateKey, start: m.start(), end: text.len() });
    }
    for c in p.id.captures_iter(text) {
        let g = c.get(1).unwrap();
        if national_id_valid(g.as_str()) {
            out.push(Finding { kind: SecretKind::NationalId, start: g.start(), end: g.end() });
        }
    }
    for c in p.card.captures_iter(text) {
        let g = c.get(1).unwrap();
        if luhn_valid(g.as_str()) {
            out.push(Finding { kind: SecretKind::BankCard, start: g.start(), end: g.end() });
        }
    }
    for c in p.password.captures_iter(text) {
        let g = c.get(1).unwrap();
        out.push(Finding { kind: SecretKind::Password, start: g.start(), end: g.end() });
    }
    for c in p.otp.captures_iter(text) {
        let g = c.get(1).unwrap();
        out.push(Finding { kind: SecretKind::OneTimeCode, start: g.start(), end: g.end() });
    }
    out.sort_by_key(|f| (f.start, std::cmp::Reverse(f.end)));
    let mut merged: Vec<Finding> = Vec::new();
    for f in out {
        if let Some(last) = merged.last() {
            if f.start < last.end {
                continue;
            }
        }
        merged.push(f);
    }
    merged
}

/// True if `text` contains anything that looks like a secret.
pub fn contains_secret(text: &str) -> bool {
    !detect(text).is_empty()
}

/// Distinct kinds found, for error messages that must not echo the secret.
pub fn kinds(text: &str) -> Vec<SecretKind> {
    let mut k: Vec<SecretKind> = detect(text).into_iter().map(|f| f.kind).collect();
    k.dedup();
    k
}

/// Replace every secret with `[REDACTED:<kind>]`.
pub fn redact(text: &str) -> String {
    let findings = detect(text);
    if findings.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut pos = 0;
    for f in findings {
        out.push_str(&text[pos..f.start]);
        out.push_str(&format!("[REDACTED:{}]", f.kind.as_str()));
        pos = f.end;
    }
    out.push_str(&text[pos..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_keys_and_private_keys() {
        assert_eq!(kinds("key sk-abcdefghijklmnopqrstuv"), vec![SecretKind::ApiKey]);
        assert_eq!(kinds("ghp_ABCDEFGHIJKLMNOPQRSTUVWX"), vec![SecretKind::ApiKey]);
        assert!(contains_secret("-----BEGIN RSA PRIVATE KEY-----\nMIIE"));
    }

    #[test]
    fn otp_in_chinese_without_word_boundary() {
        let t = "【某银行】您的验证码是483920，5分钟内有效";
        assert_eq!(kinds(t), vec![SecretKind::OneTimeCode]);
        let r = redact(t);
        assert!(!r.contains("483920"));
        assert!(r.contains("[REDACTED:one_time_code]"));
        assert!(contains_secret("Your verification code: 1234"));
    }

    #[test]
    fn passwords() {
        assert!(contains_secret("邮箱授权码：ABCD1234EFGH"));
        assert!(contains_secret("password = hunter22"));
        assert!(!contains_secret("请修改密码策略"));
    }

    #[test]
    fn cards_need_luhn() {
        assert!(luhn_valid("4111111111111111"));
        assert!(contains_secret("卡号4111111111111111请核对"));
        // 16 digits failing Luhn: an order number, not a card.
        assert!(!contains_secret("订单号 4111111111111112"));
    }

    #[test]
    fn national_id_checksum() {
        assert!(national_id_valid("11010519491231002X"));
        assert!(contains_secret("身份证11010519491231002X"));
        assert!(!contains_secret("编号110105194912310021"));
    }

    #[test]
    fn plain_text_untouched() {
        let t = "明天下午三点在 3 号会议室开会，议程见附件。";
        assert_eq!(redact(t), t);
    }
}
