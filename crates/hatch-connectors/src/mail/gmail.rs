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

use super::gmail_parse::{parse_read_message, parse_send_id, parse_triage_list};
use super::{MailSender, MailSource, OutgoingMail, ParsedMail};
use hatch_core::{Error, Result};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Default helper binary (the Gmail connector's documented entry point).
pub const DEFAULT_GWS_CLI: &str = "/opt/hatch/bin/hatch_gws_cli";
/// Hard cap on mails per sync: a pilot run must stay small.
pub const MAX_FETCH: usize = 20;
/// A single helper call may run this long before it is killed.
const TRIAGE_TIMEOUT: Duration = Duration::from_secs(120);
const READ_TIMEOUT: Duration = Duration::from_secs(60);
const SEND_TIMEOUT: Duration = Duration::from_secs(120);

/// How the Gmail helpers are invoked. The production runner shells
/// out to the helper binary; tests substitute canned JSON.
pub trait GmailCliRunner: Send + Sync {
    fn run(&self, argv: &[String], timeout: Duration) -> Result<String>;
}

/// Runs the helper binary as a subprocess (no shell, so subjects and
/// bodies need no quoting). Stdout/stderr are drained on threads so a
/// large reply cannot deadlock the worker, and the call is killed on
/// timeout instead of hanging the worker forever.
pub struct ProcessRunner {
    cli: PathBuf,
}

impl ProcessRunner {
    pub fn new(cli: PathBuf) -> Self {
        ProcessRunner { cli }
    }
}

impl GmailCliRunner for ProcessRunner {
    fn run(&self, argv: &[String], timeout: Duration) -> Result<String> {
        let mut child = Command::new(&self.cli)
            .args(argv)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::Connector(format!("启动 Gmail helper 失败：{e}")))?;
        let mut out_pipe = child.stdout.take();
        let mut err_pipe = child.stderr.take();
        let out_thread = std::thread::spawn(move || read_all(&mut out_pipe).unwrap_or_default());
        let err_thread = std::thread::spawn(move || read_all(&mut err_pipe).unwrap_or_default());
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(Error::Connector("Gmail helper 超时（已终止）".into()));
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => return Err(Error::Connector(format!("等待 Gmail helper 失败：{e}"))),
            }
        }
        let status = child
            .wait()
            .map_err(|e| Error::Connector(format!("等待 Gmail helper 失败：{e}")))?;
        let stdout = out_thread.join().unwrap_or_default();
        let stderr = err_thread.join().unwrap_or_default();
        if !status.success() {
            return Err(Error::Connector(format!(
                "Gmail helper 退出码 {status}：{}",
                String::from_utf8_lossy(&stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&stdout).into_owned())
    }
}

fn read_all(pipe: &mut Option<impl Read>) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    if let Some(p) = pipe.as_mut() {
        p.read_to_end(&mut buf)?;
    }
    Ok(buf)
}

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
        std::env::var("HATCH_GWS_CLI")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_GWS_CLI))
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
        let out = self.runner.run(&argv, SEND_TIMEOUT)?;
        Ok(parse_send_id(&out))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
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
}
