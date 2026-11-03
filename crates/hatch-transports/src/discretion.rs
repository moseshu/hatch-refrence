//! Per-chat sharing rules (`DISCRETION.md`) and the outbound check.
//!
//! One file per chat, `$HATCH_HOME/chats/<chat-id>/DISCRETION.md`:
//!
//! ```text
//! # 每行一条规则：forbid: <类别> 或 allow: <类别>
//! # 类别：mail（邮件内容）、other_people（他人的消息）、
//! #       secrets（秘密）、files（工作区文件内容）
//! forbid: mail
//! forbid: other_people
//! ```
//!
//! Rules not mentioned are allowed; a missing file means "no extra
//! restrictions". **Secrets are blocked for every chat, rules or not** —
//! the secret scan runs before anything else and a hit blocks the
//! whole message (secrets are never redacted-and-sent, only refused).
//!
//! [`check_outbound`] runs before **any** outbound message:
//!
//! 1. secret scan hit, or a `secrets` label → [`CheckOutcome::Block`];
//! 2. a label naming a forbidden category → `Block` (the producer
//!    declared what the draft is made of; the whole draft is refused);
//! 3. otherwise, line-pattern redaction for the forbidden categories
//!    that have patterns (`mail`, `other_people`): matching lines are
//!    replaced with a placeholder and the rest is sent as
//!    [`CheckOutcome::Redact`]; if nothing sendable remains → `Block`.
//!
//! The patterns are heuristics, and documented as such: they catch
//! quoted mail headers and "`X 说：`" attributions, not every way of
//! paraphrasing mail. The label path (2) is the reliable one — the
//! turn runner labels a turn by the tools it used. `files` has no
//! pattern and relies on labels alone.

use crate::message::ContentKind;
use hatch_core::{Error, Result};

/// Parsed sharing rules for one chat.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiscretionRules {
    /// Categories this chat must not receive.
    pub forbidden: Vec<ContentKind>,
}

impl DiscretionRules {
    /// No extra restrictions (a missing DISCRETION.md). Secrets are
    /// still blocked by the check itself, not by this list.
    pub fn open() -> Self {
        DiscretionRules::default()
    }

    pub fn forbids(&self, kind: ContentKind) -> bool {
        self.forbidden.contains(&kind)
    }

    /// Parse a DISCRETION.md body. Unknown categories and malformed
    /// lines are errors — a typo'd rule must fail loudly, never
    /// silently allow mail into a chat that was meant to forbid it.
    pub fn parse(text: &str) -> Result<DiscretionRules> {
        let mut rules = DiscretionRules::default();
        for (i, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (verb, rest) = line
                .split_once(':')
                .ok_or_else(|| Error::Invalid(format!("DISCRETION.md 第 {} 行无法解析（应为 `forbid: <类别>` 或 `allow: <类别>`）：{line}", i + 1)))?;
            let kind = ContentKind::parse(rest.trim()).map_err(|e| {
                Error::Invalid(format!("DISCRETION.md 第 {} 行：{e}", i + 1))
            })?;
            match verb.trim() {
                "forbid" => {
                    if !rules.forbidden.contains(&kind) {
                        rules.forbidden.push(kind);
                    }
                }
                "allow" => rules.forbidden.retain(|k| k != &kind),
                other => {
                    return Err(Error::Invalid(format!(
                        "DISCRETION.md 第 {} 行：未知的动词 `{other}`（只支持 forbid / allow）",
                        i + 1
                    )))
                }
            }
        }
        Ok(rules)
    }

    /// Serialise back to DISCRETION.md form (what `set` writes).
    pub fn render(&self) -> String {
        let mut s = String::from(
            "# 这个聊天的分享规则（每行一条：forbid: <类别> 或 allow: <类别>）\n\
             # 类别：mail（邮件内容）、other_people（他人的消息）、secrets（秘密）、files（工作区文件内容）\n\
             # 未写明的类别默认允许；secrets 永远禁止，与文件无关。\n",
        );
        for k in &self.forbidden {
            s.push_str(&format!("forbid: {}\n", k.rule_name()));
        }
        s
    }
}

/// What the discretion check decided about an outbound draft.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckOutcome {
    /// Send as-is.
    Allow,
    /// Do not send anything. `reason` names the rule, never the content.
    Block { reason: String },
    /// Send `text` instead: forbidden lines were replaced.
    /// `removed` counts the redacted lines per category.
    Redact { text: String, removed: Vec<(ContentKind, usize)> },
}

fn is_mail_header(line: &str) -> bool {
    let t = line.trim_start();
    ["发件人", "收件人", "抄送", "主题", "From:", "To:", "Cc:", "Subject:"]
        .iter()
        .any(|p| t.starts_with(p))
}

fn is_other_people_quote(line: &str) -> bool {
    // `张三说：……` / `Alice said: …` — a short attribution, then the quote.
    let t = line.trim_start();
    let Some((head, _)) = t.split_once(['：', ':']) else {
        return false;
    };
    !head.is_empty()
        && head.chars().count() <= 24
        && !head.contains([' ', '，', '。'])
        && (head.ends_with('说') || head.ends_with("said") || head.ends_with("says"))
}

/// Redact the lines of `text` that match a forbidden category's
/// pattern. Returns the redacted text and, per category, how many
/// lines were replaced.
fn redact_lines(rules: &DiscretionRules, text: &str) -> (String, Vec<(ContentKind, usize)>) {
    let mut removed: Vec<(ContentKind, usize)> = Vec::new();
    let count = |kind: ContentKind, out: &mut Vec<(ContentKind, usize)>| {
        if let Some(e) = out.iter_mut().find(|(k, _)| *k == kind) {
            e.1 += 1;
        } else {
            out.push((kind, 1));
        }
    };
    let mut lines = Vec::new();
    for line in text.lines() {
        let mut hit: Option<ContentKind> = None;
        if rules.forbids(ContentKind::Mail) && is_mail_header(line) {
            hit = Some(ContentKind::Mail);
        } else if rules.forbids(ContentKind::OtherPeople) && is_other_people_quote(line) {
            hit = Some(ContentKind::OtherPeople);
        }
        match hit {
            Some(kind) => {
                count(kind, &mut removed);
                lines.push(format!("［已按该聊天的分享规则删除{}］", kind.display()));
            }
            None => lines.push(line.to_string()),
        }
    }
    (lines.join("\n"), removed)
}

/// Rules a user mentioned casually in conversation, recognised by
/// explicit negative phrasing ("不要把邮件发到这里", "别把别人的消息
/// 发过来" …).
///
/// Muse evidence (deep-dive §9.4, E-behaviour): rules the user states
/// casually in conversation are recorded as that chat's long-term
/// rules. This matcher is the ORIGINAL, deliberately conservative
/// implementation of that behaviour: it only matches explicit
/// "不要/别 + 把/发/分享 + <类别>" phrasing, never a positive
/// sentence ("把邮件发给我看看" matches nothing), and the transport
/// tells the user what it recorded so a misfire can be corrected
/// with `hatch transport discretion set`.
pub fn extract_mentioned_rules(text: &str) -> Vec<ContentKind> {
    const PHRASES: &[(&str, ContentKind)] = &[
        ("不要把邮件", ContentKind::Mail),
        ("别把邮件", ContentKind::Mail),
        ("不要发邮件", ContentKind::Mail),
        ("别发邮件", ContentKind::Mail),
        ("不要分享邮件", ContentKind::Mail),
        ("邮件不要发", ContentKind::Mail),
        ("不要把我的邮件", ContentKind::Mail),
        ("不要把别人的消息", ContentKind::OtherPeople),
        ("别把别人的消息", ContentKind::OtherPeople),
        ("不要转发别人的", ContentKind::OtherPeople),
        ("别转发别人的", ContentKind::OtherPeople),
        ("不要把文件", ContentKind::Files),
        ("别把文件", ContentKind::Files),
        ("不要分享文件", ContentKind::Files),
        ("不要发密码", ContentKind::Secrets),
        ("别发密码", ContentKind::Secrets),
        ("不要把密码", ContentKind::Secrets),
    ];
    let mut out = Vec::new();
    for (phrase, kind) in PHRASES {
        if text.contains(phrase) && !out.contains(kind) {
            out.push(*kind);
        }
    }
    out
}

/// The outbound check. `labels` are the producer's own declaration of
/// what the draft is made of (the turn runner derives them from the
/// tools the turn used).
pub fn check_outbound(rules: &DiscretionRules, text: &str, labels: &[ContentKind]) -> CheckOutcome {
    // 1. Secrets: never sent, never redacted-and-sent, rules or not.
    if labels.contains(&ContentKind::Secrets) || hatch_core::secrets::contains_secret(text) {
        return CheckOutcome::Block { reason: "内容疑似包含秘密，任何聊天都不发送秘密".into() };
    }
    // 2. A labelled forbidden category blocks the whole draft.
    if let Some(kind) = labels.iter().find(|k| rules.forbids(**k)) {
        return CheckOutcome::Block {
            reason: format!("该聊天禁止分享{}（按 DISCRETION.md）", kind.display()),
        };
    }
    // 3. Pattern redaction for unlabelled text.
    let (redacted, removed) = redact_lines(rules, text);
    if !removed.is_empty() {
        let sendable = redacted
            .lines()
            .any(|l| !l.trim().is_empty() && !l.starts_with("［已按该聊天的分享规则删除"));
        if !sendable {
            return CheckOutcome::Block { reason: "删除被禁内容后没有可发送的内容".into() };
        }
        return CheckOutcome::Redact { text: redacted, removed };
    }
    CheckOutcome::Allow
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(text: &str) -> DiscretionRules {
        DiscretionRules::parse(text).unwrap()
    }

    #[test]
    fn parse_accepts_comments_and_allow_undoes_forbid() {
        let r = rules("# 注释\nforbid: mail\nforbid: other_people\nallow: mail\n");
        assert_eq!(r.forbidden, vec![ContentKind::OtherPeople]);
        assert!(rules("").forbidden.is_empty(), "empty file = open");
        assert!(rules("# only comments\n").forbidden.is_empty());
    }

    #[test]
    fn parse_rejects_unknown_category_and_verb() {
        assert!(DiscretionRules::parse("forbid: email\n").is_err());
        assert!(DiscretionRules::parse("block: mail\n").is_err());
        assert!(DiscretionRules::parse("forbid mail\n").is_err());
    }

    #[test]
    fn labelled_forbidden_category_blocks() {
        let r = rules("forbid: mail\n");
        let out = check_outbound(&r, "今天天气不错", &[ContentKind::Mail]);
        assert!(matches!(out, CheckOutcome::Block { .. }), "{out:?}");
        assert!(matches!(check_outbound(&r, "今天天气不错", &[]), CheckOutcome::Allow));
        let open = DiscretionRules::open();
        assert!(matches!(check_outbound(&open, "今天天气不错", &[ContentKind::Mail]), CheckOutcome::Allow));
    }

    #[test]
    fn secrets_block_for_every_chat_and_are_never_echoed() {
        let open = DiscretionRules::open();
        let out = check_outbound(&open, "我的密码是 hunter22x", &[]);
        let CheckOutcome::Block { reason } = out else {
            panic!("expected a block, got {out:?}")
        };
        assert!(!reason.contains("hunter22x"), "block reason must not echo the secret");
        let out = check_outbound(&open, "安全文本", &[ContentKind::Secrets]);
        assert!(matches!(out, CheckOutcome::Block { .. }));
    }

    #[test]
    fn mail_headers_are_redacted_and_the_rest_is_sent() {
        let r = rules("forbid: mail\n");
        let text = "老板让我转告你：\n发件人：boss@corp.com\n主题：Q3 预算\n会议改到周四。";
        let out = check_outbound(&r, text, &[]);
        let CheckOutcome::Redact { text: redacted, removed } = out else {
            panic!("expected a redact, got {out:?}")
        };
        assert_eq!(removed, vec![(ContentKind::Mail, 2)]);
        assert!(redacted.contains("会议改到周四"), "{redacted}");
        assert!(!redacted.contains("boss@corp.com") && !redacted.contains("Q3 预算"), "{redacted}");
    }

    #[test]
    fn redacted_to_nothing_blocks() {
        let r = rules("forbid: mail\n");
        let out = check_outbound(&r, "发件人：boss@corp.com\n主题：Q3 预算", &[]);
        assert!(matches!(out, CheckOutcome::Block { .. }), "{out:?}");
    }

    #[test]
    fn other_people_quotes_are_redacted() {
        let r = rules("forbid: other_people\n");
        let text = "我的看法是这样。\n张三说：他明天不来。\n你看呢？";
        let out = check_outbound(&r, text, &[]);
        let CheckOutcome::Redact { text: redacted, .. } = out else {
            panic!("expected a redact, got {out:?}")
        };
        assert!(!redacted.contains("他明天不来"), "{redacted}");
        assert!(redacted.contains("我的看法是这样"), "{redacted}");
    }

    #[test]
    fn render_round_trips() {
        let r = rules("forbid: mail\nforbid: files\n");
        let back = DiscretionRules::parse(&r.render()).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn mentioned_rules_match_only_explicit_negative_phrasing() {
        assert_eq!(
            extract_mentioned_rules("以后不要把邮件发到这个群"),
            vec![ContentKind::Mail]
        );
        assert_eq!(
            extract_mentioned_rules("别把别人的消息发过来，也不要把文件发过来"),
            vec![ContentKind::OtherPeople, ContentKind::Files]
        );
        assert!(extract_mentioned_rules("把邮件发给我看看").is_empty(), "a positive request is not a rule");
        assert!(extract_mentioned_rules("今天天气不错").is_empty());
        assert!(extract_mentioned_rules("邮件里说什么？").is_empty());
    }
}
