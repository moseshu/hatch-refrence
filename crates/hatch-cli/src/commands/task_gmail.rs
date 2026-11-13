//! Real-account (Gmail) runs of the task worker (M2 pilot).
//!
//! A Gmail run has no fixture world: the persona comes from CLI
//! flags (`--gmail-user` / `--gmail-name` / `--timezone`), the mail
//! comes from `GmailMailbox`, and the invite spec (if any) comes
//! from `--invite-*` flags. Everything else — triage, gather,
//! artifact, approvals, the task ledger — is the same worker core
//! the fixture runs use, so desensitization applies unchanged: the
//! mapper is built from the database contents, never from the mail's
//! origin, and the model only ever sees sender codes.
//!
//! The pilot is bounded by construction: `GmailConfig` caps a fetch
//! at 20 messages, and the CLI caps `--gmail-max` at 20 too, so one
//! run can never sweep the whole inbox.

use super::task_invite::InviteSpec;
use super::task_worker::{run_worker_core, RunInput, WorkerOpts, WorkerReport};
use hatch_connectors::mail::gmail::{GmailConfig, GmailMailbox, GmailSender};
use hatch_connectors::MailSender;
use hatch_core::time::{now as now_now, parse_tz, to_rfc3339, Timestamp};
use hatch_core::Result;
use hatch_eval::{Expectations, Persona, World};
use hatch_store::Db;
use std::path::Path;
use std::sync::Arc;

/// One real-account run: account, scope, and (for the invite kind)
/// the invite arguments. The CLI fills the defaults for `title` and
/// `prompt`, so the worker always has both.
pub struct GmailRun {
    /// The account's own address (also the `from` of any send).
    pub user_email: String,
    /// The account owner's display name.
    pub user_name: String,
    /// IANA timezone for dates and the gather prompt.
    pub timezone: String,
    /// Gmail search query for the sync (e.g. `is:unread`).
    pub query: String,
    /// How many recent messages to sync (1..=20).
    pub max: u64,
    /// Task title (CLI default: "简报：最近邮件" / "邀请：<标题>").
    pub title: String,
    /// Gather instruction (CLI default asks for recent decisions).
    pub prompt: String,
    /// Invite arguments; `None` for the brief kind.
    pub invite: Option<GmailInviteArgs>,
}

/// The invite's arguments, all from CLI flags.
pub struct GmailInviteArgs {
    /// Meeting title.
    pub event_title: String,
    /// "YYYY-MM-DD HH:MM" in the run's timezone.
    pub event_start: String,
    /// Meeting location (may be empty).
    pub location: String,
    /// Attendee display name.
    pub attendee_name: String,
    /// Attendee address (the real send target).
    pub attendee_email: String,
    /// 议题 line (empty omits it).
    pub topics: String,
}

/// The minimal `World` for a Gmail run: an empty mailbox, no
/// calendar, no expectations — the worker only needs the persona
/// (account, name, timezone) and the clock. The triage workflow
/// and the gather turn read mail from the database, not the world.
pub fn gmail_world(run: &GmailRun, now: &Timestamp) -> Result<World> {
    Ok(World {
        name: "gmail".to_string(),
        dir: std::path::PathBuf::new(),
        persona: Persona {
            name: run.user_name.clone(),
            email: run.user_email.clone(),
            timezone: run.timezone.clone(),
            now: to_rfc3339(now),
            vips: vec![],
            low: vec![],
            contacts: vec![],
            style: vec![],
        },
        mailbox: vec![],
        calendar_ics: String::new(),
        expectations: Expectations {
            attacker_domains: vec![],
            secret_strings: vec![],
            mails: Default::default(),
            thread: None,
            brief: None,
        },
    })
}

/// Run (or resume) a real-account task: build the minimal world,
/// a `GmailMailbox` for reading and a `GmailSender` for the invite,
/// and hand both to the shared worker core. The sender is inert
/// until an approved `send_email` step executes it — an unapproved
/// send never reaches the helper.
pub(crate) fn run_worker_gmail(
    db: &Arc<Db>,
    run: &GmailRun,
    out_dir: &Path,
    opts: WorkerOpts,
) -> Result<WorkerReport> {
    let now = opts.now_override.unwrap_or_else(now_now);
    let world = gmail_world(run, &now)?;
    let tz = parse_tz(&run.timezone)?;
    let mut source = GmailMailbox::new(GmailConfig {
        query: Some(run.query.clone()),
        max: run.max as usize,
        cli: GmailConfig::cli_path(),
    });
    let invite_spec = match &run.invite {
        Some(inv) => Some(InviteSpec::from_args(
            &inv.event_title,
            &inv.event_start,
            &inv.location,
            &inv.attendee_name,
            &inv.attendee_email,
            &inv.topics,
            &run.user_name,
            &now,
            &tz,
        )?),
        None => None,
    };
    let input = RunInput {
        title: Some(run.title.clone()),
        prompt: Some(run.prompt.clone()),
        invite_spec,
        invite_sender: Some(
            Arc::new(GmailSender::new(GmailConfig::cli_path())) as Arc<dyn MailSender>
        ),
        with_calendar: false,
        source: "gmail",
    };
    run_worker_core(db, &world, None, out_dir, opts, &mut source, input)
}
