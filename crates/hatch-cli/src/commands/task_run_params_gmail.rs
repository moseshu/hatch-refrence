//! The Gmail half of the persisted execution parameters (P0
//! product closed loop): the invite shape, its parsing, and the
//! conversion of stored parameters into the Gmail run the worker
//! executes. Split out of `task_run_params` so neither file grows
//! past the 400-line rule; the two modules are one crate, so the
//! `impl RunParams` block here is the same type.

use super::task_run_params::RunParams;
use crate::commands::task_gmail::{GmailInviteArgs, GmailRun};
use hatch_core::{Error, Result};
use serde::{Deserialize, Serialize};

/// The invite's arguments, mirroring the CLI `--invite-*` flags.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteParams {
    pub event_title: String,
    pub event_start: String,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub attendee_name: String,
    pub attendee_email: String,
    #[serde(default)]
    pub topics: String,
    /// Also create the event in the local calendar (its own
    /// approval). Absent in old rows → false.
    #[serde(default)]
    pub create_event: bool,
}

impl RunParams {
    /// Build the Gmail run these parameters describe (gmail source).
    /// Defaults mirror `gmail_run_from_flags`: the title and prompt
    /// were already defaulted at creation, so this only fills the
    /// invite arguments through.
    pub fn to_gmail_run(&self) -> Result<GmailRun> {
        let user_email = self
            .gmail_user
            .clone()
            .ok_or_else(|| Error::Invalid("source gmail 需要 gmail_user <邮箱>".into()))?;
        let user_name = self
            .gmail_name
            .clone()
            .unwrap_or_else(|| user_email.clone());
        let invite = self.invite.as_ref().map(|i| GmailInviteArgs {
            event_title: i.event_title.clone(),
            event_start: i.event_start.clone(),
            location: i.location.clone(),
            attendee_name: if i.attendee_name.is_empty() {
                i.attendee_email.clone()
            } else {
                i.attendee_name.clone()
            },
            attendee_email: i.attendee_email.clone(),
            topics: i.topics.clone(),
            create_event: i.create_event,
        });
        Ok(GmailRun {
            user_email,
            user_name,
            timezone: self.timezone.clone(),
            query: self.gmail_query.clone(),
            max: self.gmail_max,
            title: self.title.clone(),
            prompt: self
                .prompt
                .clone()
                .unwrap_or_else(|| "把最近邮件里的决定整理成简报，每条注明来源邮件".to_string()),
            invite,
        })
    }
}

/// Parse the invite object: the three required fields, the rest
/// defaulted — the same requirement as the CLI `--invite-*` flags.
pub(crate) fn parse_invite(v: &serde_json::Value) -> Result<InviteParams> {
    let need = |f: &str| {
        v.get(f)
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .ok_or_else(|| Error::Invalid(format!("invite 缺少字段 {f}（会议邀请需要）")))
    };
    Ok(InviteParams {
        event_title: need("event_title")?,
        event_start: need("event_start")?,
        location: v
            .get("location")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string(),
        attendee_name: v
            .get("attendee_name")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string(),
        attendee_email: need("attendee_email")?,
        topics: v
            .get("topics")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string(),
        create_event: v
            .get("create_event")
            .and_then(|x| x.as_bool())
            .unwrap_or(false),
    })
}

/// The title when the client did not give one: kind-based, like the
/// worker's own derivation for a fresh task.
pub(crate) fn default_title(kind: &str, invite: Option<&InviteParams>) -> String {
    match kind {
        "meeting-invite" => format!(
            "邀请：{}",
            invite.map(|i| i.event_title.as_str()).unwrap_or("")
        ),
        "delegate-join" => "汇合：子任务结果".to_string(),
        "weekly-report" => "周报".to_string(),
        _ => "简报：最近邮件".to_string(),
    }
}
