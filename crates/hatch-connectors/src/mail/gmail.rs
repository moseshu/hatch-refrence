//! Real Gmail as a mail source / sender, through the `hatch_gws_cli`
//! helper (`gmail +triage` / `+read` / `+send`).
//!
//! The helper owns the credential (the connector's OAuth grant); this
//! module never sees a key, a token or a password — it only passes
//! argv to the helper binary and parses its JSON output. The helper
//! path comes from `HATCH_GWS_CLI` (default `/opt/hatch/bin/hatch_gws_cli`).
//!
//! Scope is always bounded: `+triage --max N` caps the list, and at
//! most that many `+read` calls follow. Gmail has no UIDs, so the
//! sync cursor never advances; re-fetching is harmless (the mail repo
//! stores by stable id, `INSERT OR IGNORE`).
//!
//! `+triage` / `+read` output shapes are parsed tolerantly (several
//! key spellings accepted) because the helper's JSON is only
//! documented by example (`jq '.[].subject'`, `jq '.body_text'`).

use super::gmail_parse::{
    parse_message_list_ids, parse_read_message, parse_send_id, parse_triage_list,
};
use super::{MailSender, MailSource, OutgoingMail, ParsedMail};
use hatch_core::Result;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The helper runner lives in `crate::gws_cli` (shared with Google
/// Calendar); the Gmail names stay as aliases so existing callers
/// and tests do not change.
pub use crate::gws_cli::{CliRunner as GmailCliRunner, ProcessRunner, DEFAULT_GWS_CLI};

/// Hard cap on mails per sync: a pilot run must stay small.
pub const MAX_FETCH: usize = 20;
/// A single helper call may run this long before it is killed.
const TRIAGE_TIMEOUT: Duration = Duration::from_secs(120);
const READ_TIMEOUT: Duration = Duration::from_secs(60);
const SEND_TIMEOUT: Duration = Duration::from_secs(120);

/// Gmail source configuration. No credential lives here.
#[derive(Clone, Debug)]
pub struct GmailConfig {
    /// Gmail search query for the triage list (default `is:unread`).
    pub query: Option<String>,
    /// Max mails per sync (default 10, hard-capped at `MAX_FETCH`).
    pub max: usize,
    /// Helper binary path.
    pub cli: PathBuf,
}

impl Default for GmailConfig {
    fn default() -> Self {
        GmailConfig {
            query: None,
            max: 10,
            cli: Self::cli_path(),
        }
    }
}

impl GmailConfig {
    pub fn cli_path() -> PathBuf {
        crate::gws_cli::cli_path()
    }
}

/// The real mailbox: `+triage` for the list, `+read` per message.
pub struct GmailMailbox {
    cfg: GmailConfig,
    runner: Arc<dyn GmailCliRunner>,
}

impl GmailMailbox {
    pub fn new(cfg: GmailConfig) -> Self {
        let cli = cfg.cli.clone();
        GmailMailbox {
            cfg,
            runner: Arc::new(ProcessRunner::new(cli)),
        }
    }

    pub fn with_runner(cfg: GmailConfig, runner: Arc<dyn GmailCliRunner>) -> Self {
        GmailMailbox { cfg, runner }
    }
}

impl MailSource for GmailMailbox {
    fn account(&self) -> String {
        "gmail".to_string()
    }

    fn fetch_new(&mut self, _after_uid: Option<u32>, limit: usize) -> Result<Vec<ParsedMail>> {
        // Gmail has no UIDs: every sync re-lists the most recent N.
        // The repo dedups by stable id, so re-fetching is harmless.
        let n = limit.min(self.cfg.max).min(MAX_FETCH);
        if n == 0 {
            return Ok(vec![]);
        }
        let mut argv: Vec<String> = vec![
            "gmail".into(),
            "+triage".into(),
            "--max".into(),
            n.to_string(),
            "--format".into(),
            "json".into(),
        ];
        if let Some(q) = &self.cfg.query {
            argv.push("--query".into());
            argv.push(q.clone());
        }
        let items = parse_triage_list(&self.runner.run(&argv, TRIAGE_TIMEOUT)?)?;
        let mut mails = Vec::new();
        for item in items.into_iter().take(n) {
            let argv: Vec<String> = vec![
                "gmail".into(),
                "+read".into(),
                "--id".into(),
                item.id.clone(),
                "--headers".into(),
                "--format".into(),
                "json".into(),
            ];
            let out = self.runner.run(&argv, READ_TIMEOUT)?;
            mails.push(parse_read_message(&out, &item.id)?);
        }
        Ok(mails)
    }
}

/// The real sender: `+send`. Only ever constructed for a real-account
/// task, and only ever invoked through the approval-gated
/// `SendEmailExecutor` — an unapproved send never reaches here.
pub struct GmailSender {
    runner: Arc<dyn GmailCliRunner>,
}

impl GmailSender {
    pub fn new(cli: PathBuf) -> Self {
        GmailSender {
            runner: Arc::new(ProcessRunner::new(cli)),
        }
    }

    pub fn with_runner(runner: Arc<dyn GmailCliRunner>) -> Self {
        GmailSender { runner }
    }
}

impl MailSender for GmailSender {
    fn read_sent(&self, message_id: &str) -> Result<ParsedMail> {
        // gmail users.messages.get verifies both the provider id and SENT label.
        let argv = vec!["gmail".into(), "users".into(), "messages".into(), "get".into(),
            "--params".into(), serde_json::json!({"userId":"me", "id":message_id, "format":"raw"}).to_string()];
        let out = self.runner.run(&argv, READ_TIMEOUT)?;
        super::sent_read::parse_gmail_sent(&out, message_id)
    }

    fn send(&self, mail: &OutgoingMail) -> Result<String> {
        let mut argv: Vec<String> = vec![
            "gmail".into(),
            "+send".into(),
            "--to".into(),
            mail.to.join(","),
            "--subject".into(),
            mail.subject.clone(),
            "--body".into(),
            mail.body.clone(),
            "--format".into(),
            "json".into(),
        ];
        if !mail.from.is_empty() {
            argv.push("--from".into());
            argv.push(mail.from.clone());
        }
        let started_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        match self.runner.run(&argv, SEND_TIMEOUT) {
            Ok(out) => Ok(parse_send_id(&out)),
            Err(send_err) => {
                // A failed call does not prove the mail was not sent:
                // the helper can be killed on timeout after Gmail
                // already accepted the message. Reconcile against the
                // sent mailbox before declaring failure — claiming
                // failure here invites a duplicate resend.
                match self.reconcile_sent(mail, started_unix) {
                    Ok(Some(id)) => Ok(id),
                    _ => Err(send_err),
                }
            }
        }
    }
}

impl GmailSender {
    /// Look for this exact message in the sent mailbox: same
    /// recipients, subject and body, sent no earlier than this
    /// attempt (with a small clock-skew allowance). Returns the
    /// provider id when found; the caller's verification gate then
    /// re-reads it independently as usual.
    fn reconcile_sent(&self, mail: &OutgoingMail, started_unix: u64) -> Result<Option<String>> {
        let Some(first_to) = mail.to.first() else {
            return Ok(None);
        };
        let after = started_unix.saturating_sub(120);
        let query = format!("in:sent to:{first_to} after:{after}");
        let argv: Vec<String> = vec![
            "gmail".into(),
            "users".into(),
            "messages".into(),
            "list".into(),
            "--params".into(),
            serde_json::json!({"userId": "me", "q": query, "maxResults": 10}).to_string(),
        ];
        let out = self.runner.run(&argv, READ_TIMEOUT)?;
        for id in parse_message_list_ids(&out) {
            let Ok(sent) = self.read_sent(&id) else {
                continue;
            };
            if sent.subject == mail.subject
                && same_recipients(&sent.to, &mail.to)
                && sent.body.trim() == mail.body.trim()
            {
                return Ok(Some(id));
            }
        }
        Ok(None)
    }
}

/// Bare lowercase address from a `Name <addr>` or plain `addr` entry.
fn normalize_addr(raw: &str) -> String {
    let addr = match (raw.rfind('<'), raw.rfind('>')) {
        (Some(open), Some(close)) if close > open => &raw[open + 1..close],
        _ => raw,
    };
    addr.trim().to_ascii_lowercase()
}

fn same_recipients(a: &[String], b: &[String]) -> bool {
    let mut a: Vec<String> = a.iter().map(|s| normalize_addr(s)).collect();
    let mut b: Vec<String> = b.iter().map(|s| normalize_addr(s)).collect();
    a.sort();
    b.sort();
    a == b
}
#[cfg(test)]
mod tests {
    use super::*;
    use hatch_core::Error;
    use std::sync::Mutex;

    /// Canned runner: answers by substring match on the joined argv,
    /// and records every call for assertions.
    struct CannedRunner {
        responses: Vec<(String, String)>,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl CannedRunner {
        fn new(responses: Vec<(String, String)>) -> Self {
            CannedRunner {
                responses,
                calls: Mutex::new(vec![]),
            }
        }
    }

    impl GmailCliRunner for CannedRunner {
        fn run(&self, argv: &[String], _timeout: Duration) -> Result<String> {
            self.calls.lock().unwrap().push(argv.to_vec());
            let joined = argv.join(" ");
            for (pat, resp) in &self.responses {
                if joined.contains(pat.as_str()) {
                    return Ok(resp.clone());
                }
            }
            Err(Error::Connector(format!(
                "canned runner: no response for {joined}"
            )))
        }
    }

    fn triage_json() -> String {
        serde_json::json!([
            {"id": "m1", "from": "李婷 <li.ting@corp.example>", "subject": "明天评审会：方案 B 通过", "date": "2026-11-03T09:00:00+08:00"},
            {"id": "m2", "from": "王宇 <wang.yu@corp.example>", "subject": "预算确认", "date": "2026-11-03T10:00:00+08:00"},
            {"id": "m3", "from": "韩梅梅 <han.mei@partner.example>", "subject": "合作方确认", "date": "2026-11-03T11:00:00+08:00"}
        ])
        .to_string()
    }

    fn read_json(id: &str) -> String {
        serde_json::json!({
            "id": id,
            "from": "李婷 <li.ting@corp.example>",
            "to": ["moseshu25@gmail.com"],
            "subject": "明天评审会：方案 B 通过",
            "date": "2026-11-03T09:00:00+08:00",
            "body_text": "评审会决定：方案 B 通过。"
        })
        .to_string()
    }

    #[test]
    fn triage_list_parses_and_fetch_is_capped() {
        let items = parse_triage_list(&triage_json()).unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].id, "m1");
        // max=2 caps both the list and the reads.
        let runner = Arc::new(CannedRunner::new(vec![
            ("+triage".into(), triage_json()),
            ("+read".into(), read_json("m1")),
        ]));
        let cfg = GmailConfig {
            query: Some("from:li.ting".into()),
            max: 2,
            cli: PathBuf::from("gws"),
        };
        let mut mb = GmailMailbox::with_runner(cfg, runner.clone());
        let mails = mb.fetch_new(None, 100).unwrap();
        assert_eq!(mails.len(), 2);
        assert_eq!(mails[0].from_addr, "li.ting@corp.example");
        assert_eq!(mails[0].subject, "明天评审会：方案 B 通过");
        assert_eq!(mails[0].body, "评审会决定：方案 B 通过。");
        let calls = runner.calls.lock().unwrap();
        assert_eq!(
            calls
                .iter()
                .filter(|a| a.contains(&"+read".to_string()))
                .count(),
            2
        );
        assert!(calls[0].contains(&"--max".to_string()));
        assert!(calls[0].contains(&"2".to_string()));
        assert!(calls[0].contains(&"from:li.ting".to_string()));
    }

    #[test]
    fn send_goes_through_the_helper_and_returns_its_id() {
        let runner = Arc::new(CannedRunner::new(vec![(
            "+send".into(),
            serde_json::json!({"id": "18f1a2b3c4d"}).to_string(),
        )]));
        let sender = GmailSender::with_runner(runner.clone());
        let id = sender
            .send(&OutgoingMail {
                from: "moseshu25@gmail.com".into(),
                to: vec!["li.ting@corp.example".into()],
                subject: "邀请：评审会".into(),
                body: "请参加。".into(),
                in_reply_to: None,
                references: vec![],
            })
            .unwrap();
        assert_eq!(id, "18f1a2b3c4d");
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].contains(&"--to".to_string()));
        assert!(calls[0].contains(&"li.ting@corp.example".to_string()));
        assert!(!calls[0].contains(&"+draft".to_string()));
        assert_eq!(parse_send_id("not json at all"), "not json at all");
        assert_eq!(parse_send_id(""), "gmail-sent");
    }

    fn outgoing() -> OutgoingMail {
        OutgoingMail {
            from: "moseshu25@gmail.com".into(),
            to: vec!["li.ting@corp.example".into()],
            subject: "Invite: review".into(),
            body: "Please attend.".into(),
            in_reply_to: None,
            references: vec![],
        }
    }

    /// `users messages get` (format raw) response carrying `body`.
    fn sent_get_json(id: &str, body: &str) -> String {
        use base64::Engine as _;
        let rfc822 = format!(
            "From: moseshu25@gmail.com\r\nTo: li.ting@corp.example\r\nSubject: Invite: review\r\n\r\n{body}"
        );
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(rfc822.as_bytes());
        serde_json::json!({"id": id, "labelIds": ["SENT"], "raw": raw}).to_string()
    }

    #[test]
    fn send_reconciles_when_helper_fails_but_mail_was_sent() {
        // No canned "+send" response: the helper call errors (as on a
        // timeout kill), but the message is in the sent mailbox.
        let runner = Arc::new(CannedRunner::new(vec![
            (
                "messages list".into(),
                serde_json::json!({"messages": [{"id": "sent1", "threadId": "sent1"}]})
                    .to_string(),
            ),
            (
                "messages get".into(),
                sent_get_json("sent1", "Please attend."),
            ),
        ]));
        let sender = GmailSender::with_runner(runner.clone());
        let id = sender.send(&outgoing()).unwrap();
        assert_eq!(id, "sent1");
        let calls = runner.calls.lock().unwrap();
        assert!(calls.iter().any(|a| a.contains(&"list".to_string())));
        assert!(calls.iter().any(|a| a.contains(&"get".to_string())));
    }

    #[test]
    fn send_failure_stays_failure_when_nothing_was_sent() {
        let runner = Arc::new(CannedRunner::new(vec![(
            "messages list".into(),
            serde_json::json!({"resultSizeEstimate": 0}).to_string(),
        )]));
        let sender = GmailSender::with_runner(runner);
        let err = sender.send(&outgoing()).unwrap_err().to_string();
        assert!(err.contains("canned runner"), "{err}");
    }

    #[test]
    fn send_reconcile_rejects_a_different_message() {
        // A sent message to the same person with the same subject but
        // a different body is not this send's receipt.
        let runner = Arc::new(CannedRunner::new(vec![
            (
                "messages list".into(),
                serde_json::json!({"messages": [{"id": "sent1", "threadId": "sent1"}]})
                    .to_string(),
            ),
            (
                "messages get".into(),
                sent_get_json("sent1", "Something else entirely."),
            ),
        ]));
        let sender = GmailSender::with_runner(runner);
        assert!(sender.send(&outgoing()).is_err());
    }
}
