//! Outbound heuristic: does a tool call send something to another person
//! or service, judging by the tool's name?
//!
//! The plan class is declared by the tool itself — or, for MCP and
//! connector tools, by a third-party server or manifest — so a mislabelled
//! class can let a send through as a silent read. The name is a second,
//! independent signal. This module implements the name side, following the
//! shape of OpenMausBot's `shared/outbound.ts` (`isOutboundTool` /
//! `connectorCallsIn`) re-expressed for hatch's snake_case registry names:
//!
//! * a send verb anywhere in the action tokens (`send`, `post`, `pay`, …)
//!   means the call sends;
//! * creating a thing another person receives (`create` + `message` /
//!   `comment` / `email` / …) also sends — but creating a record
//!   (`create_issue`, `create_event`, `create_page`) does not;
//! * a read verb first (`get_post`, `list_messages`) means the rest of
//!   the name is what is being read, not sent;
//! * a draft (`create_draft_email`, `propose_email`) never sends;
//! * opaque calls (`shell_run`: an arbitrary shell command can send
//!   anything) are always outbound; a wrapper name with no inner tool
//!   (`mcp_<server>` with nothing after it) fails closed the same way.
//!
//! Hatch has no multi-execute tool, so there is no argument-level
//! expansion to do: the "composite" surface is the wrapper name itself.
//! `mcp_<server>_<tool>` and `connector_<connector>_<op>` are unwrapped
//! to the inner tool/op name before tokenising; anything else is read
//! as-is. A bare name is never stripped of a leading segment: hatch's
//! built-ins are verb-first (`shell_run`, `send_email`), so dropping the
//! first segment would destroy the verb.
//!
//! [`recheck_class`] is the dispatch-side use: after `tool.plan()`, a
//! name that says "send" upgrades a plan class that could otherwise run
//! without the user to `SendMessage`. The upgrade is max-strictness, not
//! replacement — a class that already needs the user (`Draft`/`Suggest`
//! by default: `SendMessage`, `WriteCalendar`, `DeleteData`, `Payment`,
//! `ExecCommand`, `BrowserSubmit`, `WriteFile`) is left alone, because
//! it carries its own protections: the safety floor keys on
//! `ExecCommand`, and `Payment` is suggest-only. Replacing the class
//! would silently drop those.

use crate::action::{ActionClass, Autonomy};

/// Verbs whose action sends something somewhere (or moves money).
const SEND_VERBS: &[&str] = &[
    "send",
    "sends",
    "reply",
    "respond",
    "forward",
    "post",
    "publish",
    "tweet",
    "retweet",
    "broadcast",
    "invite",
    "share",
    "refund",
    "charge",
    "pay",
    "payout",
    "transfer",
    "notify",
];

/// Verbs that only count as sending when paired with a [`SENT_NOUNS`]
/// noun: creating a comment sends it, creating an issue files it.
const CREATE_VERBS: &[&str] = &["create", "creation", "add", "new"];

/// Things another person receives when they are created.
const SENT_NOUNS: &[&str] = &[
    "message", "comment", "post", "email", "mail", "invoice", "payment", "refund", "reply",
    "tweet", "dm",
];

/// A read verb first means the rest of the name is what is being read.
const READ_VERBS: &[&str] = &[
    "get", "list", "fetch", "search", "read", "find", "retrieve", "lookup", "download", "view",
    "check", "count",
];

/// Markers for a call that only prepares something to send later.
const DRAFT_MARKERS: &[&str] = &["draft", "propose"];

/// Tools whose arguments are opaque: nothing in the call says what they
/// will do, so they are outbound by definition.
const OPAQUE_TOOLS: &[&str] = &["shell_run"];

/// The action part of a tool name, as lower-case word tokens. A wrapper
/// name drops its wrapper: `mcp_<server>_<tool>` and
/// `connector_<connector>_<op>` reduce to the inner tool/op. CamelCase
/// names from MCP servers (`sendMessage`) split at the case boundary.
fn action_tokens(name: &str) -> Vec<String> {
    let mut parts: Vec<&str> = name
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
        .collect();
    if parts.len() >= 2 && (parts[0] == "mcp" || parts[0] == "connector") {
        parts.drain(..2);
    }
    let mut tokens = Vec::new();
    for part in parts {
        let mut start = 0;
        let chars: Vec<char> = part.chars().collect();
        for i in 1..chars.len() {
            if chars[i].is_ascii_uppercase()
                && (chars[i - 1].is_ascii_lowercase() || chars[i - 1].is_ascii_digit())
            {
                tokens.push(
                    chars[start..i]
                        .iter()
                        .collect::<String>()
                        .to_ascii_lowercase(),
                );
                start = i;
            }
        }
        tokens.push(
            chars[start..]
                .iter()
                .collect::<String>()
                .to_ascii_lowercase(),
        );
    }
    tokens
}

/// Does this tool send something to someone, or move money, judging by
/// its name alone? Unknown names default to **not** outbound: the name
/// alone must not invent sends for tools it cannot classify.
pub fn is_outbound_tool_name(name: &str) -> bool {
    if OPAQUE_TOOLS.contains(&name) {
        return true;
    }
    let tokens = action_tokens(name);
    if tokens.is_empty() {
        // A wrapper name with no inner tool (`mcp_<server>` and nothing
        // after it) cannot be classified: fail closed.
        return true;
    }
    if READ_VERBS.contains(&tokens[0].as_str()) {
        return false;
    }
    if tokens.iter().any(|t| SEND_VERBS.contains(&t.as_str())) {
        return true;
    }
    if tokens.iter().any(|t| DRAFT_MARKERS.contains(&t.as_str())) {
        return false;
    }
    tokens.iter().any(|t| CREATE_VERBS.contains(&t.as_str()))
        && tokens.iter().any(|t| SENT_NOUNS.contains(&t.as_str()))
}

/// The class dispatch should actually enforce for a call named `name`
/// whose plan says `class`. Never downgrades: a `SendMessage` plan stays
/// `SendMessage`, and a class that already needs the user stays itself.
pub fn recheck_class(name: &str, class: ActionClass) -> ActionClass {
    if is_outbound_tool_name(name) && class.default_autonomy() > Autonomy::Draft {
        ActionClass::SendMessage
    } else {
        class
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_verbs_flagged() {
        for n in [
            "send_email",
            "send_message",
            "mcp_gmail_send_message",
            "mcp_slack_post_message",
            "mcp_gmail_reply",
            "connector_slack_post_message",
            "mcp_github_invite_collaborator",
            "mcp_stripe_create_refund",
            "mcp_slack_notify_channel",
            "sendMessage",
            "mcp_gmail_sendMessage",
        ] {
            assert!(is_outbound_tool_name(n), "{n}");
        }
    }

    #[test]
    fn create_plus_sent_noun_flagged_record_keeping_not() {
        for n in [
            "create_comment",
            "mcp_github_create_issue_comment",
            "connector_jira_add_comment",
            "mcp_mail_new_email",
        ] {
            assert!(is_outbound_tool_name(n), "{n}");
        }
        for n in [
            "mcp_github_create_issue",
            "mcp_linear_create_issue",
            "mcp_calendar_create_event",
            "mcp_notion_create_page",
            "create_event",
        ] {
            assert!(!is_outbound_tool_name(n), "{n}");
        }
    }

    #[test]
    fn read_first_and_draft_exempt() {
        for n in [
            "get_post",
            "mcp_reddit_get_post",
            "mcp_gmail_list_messages",
            "mcp_gmail_fetch_emails",
            "mcp_gmail_search_threads",
            "create_draft_email",
            "mcp_gmail_draft_email",
            "propose_email",
        ] {
            assert!(!is_outbound_tool_name(n), "{n}");
        }
    }

    #[test]
    fn opaque_and_malformed_fail_closed() {
        assert!(is_outbound_tool_name("shell_run"));
        assert!(is_outbound_tool_name("mcp_gmail"));
        assert!(is_outbound_tool_name("connector_slack"));
    }

    #[test]
    fn unknown_names_not_outbound() {
        for n in [
            "get_weather",
            "calculate_tax",
            "memory_search",
            "fs_read",
            "web_fetch",
            "browser_click",
            "browser_open",
            "calendar_list",
            "mail_search",
            "list_skills",
            "approval_status",
        ] {
            assert!(!is_outbound_tool_name(n), "{n}");
        }
    }

    #[test]
    fn recheck_upgrades_quiet_classes_only() {
        use ActionClass::*;
        // A quiet class carrying a send name becomes SendMessage.
        for c in [
            ReadPersonal,
            FetchWeb,
            BrowseLocal,
            WriteMemory,
            DraftMessage,
        ] {
            assert_eq!(
                recheck_class("mcp_gmail_send_message", c),
                SendMessage,
                "{c:?}"
            );
        }
        // A class that already needs the user keeps its own protections:
        // the safety floor keys on ExecCommand, Payment is suggest-only.
        assert_eq!(recheck_class("shell_run", ExecCommand), ExecCommand);
        assert_eq!(recheck_class("mcp_stripe_create_refund", Payment), Payment);
        assert_eq!(recheck_class("send_email", SendMessage), SendMessage);
        assert_eq!(
            recheck_class("mcp_calendar_create_event", WriteCalendar),
            WriteCalendar
        );
        // A quiet class with a non-send name is untouched.
        assert_eq!(recheck_class("mcp_gmail_fetch_emails", FetchWeb), FetchWeb);
        assert_eq!(recheck_class("memory_search", ReadPersonal), ReadPersonal);
    }
}
