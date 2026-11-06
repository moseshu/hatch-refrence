//! Unanswered-approval annotation (OMB #4).
//!
//! A tool call that needs approval only queues the action: the tool
//! result tells the model "submitted, not yet executed", but nothing in
//! the turn's record says the action never ran — so a later answer can
//! report it as done. At turn end, every approval this turn raised that
//! is still pending gets one line in the session, naming the tool and
//! the action, telling the model not to report it as done. The note is
//! a session message, so the next round sees it in history; it is not
//! part of the user-visible answer and changes nothing in `TurnReport`:
//! what counts as done is still what actually executed.

use crate::session::SessionStore;
use hatch_core::time::Timestamp;
use hatch_core::Trust;
use hatch_llm::ChatMessage;
use hatch_policy::{ApprovalQueue, ApprovalStatus};
use hatch_store::Db;

/// Most actions named in one note; further ones are counted, not listed.
const MAX_ACTIONS: usize = 10;
/// Longest title kept in one entry (titles may hold user data).
const MAX_ENTRY_CHARS: usize = 120;

/// One approval raised this turn and still unanswered at turn end.
#[derive(Clone, Debug, PartialEq)]
pub struct UnansweredAction {
    pub id: String,
    pub kind: String,
    pub title: String,
}

/// Keep, from this turn's approval ids, the ones whose current status
/// is still `Pending`. An approval decided since it was raised —
/// approved, rejected, expired, executed, failed — is answered, not
/// unanswered, and is left out. Duplicate ids (the same action called
/// twice maps to one approval) are kept once; a row that cannot be read
/// is skipped rather than claimed.
pub fn collect_unanswered(db: &Db, ids: &[String]) -> Vec<UnansweredAction> {
    let q = ApprovalQueue::new(db);
    let mut out: Vec<UnansweredAction> = Vec::new();
    for id in ids {
        if out.iter().any(|a| &a.id == id) {
            continue;
        }
        let Ok(a) = q.get(id) else { continue };
        if a.status != ApprovalStatus::Pending {
            continue;
        }
        out.push(UnansweredAction {
            id: id.clone(),
            kind: a.kind,
            title: a.title,
        });
    }
    out
}

/// The note, or `None` when nothing is unanswered. The note names each
/// action (executor kind + title) so the model can match it to the call
/// it made.
pub fn render_note(actions: &[UnansweredAction]) -> Option<String> {
    if actions.is_empty() {
        return None;
    }
    let mut lines = vec!["本轮以下动作已提交审批、到本轮结束仍未获批准，因此未执行：".to_string()];
    for a in actions.iter().take(MAX_ACTIONS) {
        let title: String = a.title.chars().take(MAX_ENTRY_CHARS).collect();
        if title.trim().is_empty() {
            lines.push(format!("- {}", a.kind));
        } else {
            lines.push(format!("- {}（{}）", a.kind, title.trim()));
        }
    }
    if actions.len() > MAX_ACTIONS {
        lines.push(format!("- 等 {} 项。", actions.len() - MAX_ACTIONS));
    }
    lines.push(
        "不要向用户汇报成已完成；它们仍在等审批。如用户问起，请如实说明还在等审批。".to_string(),
    );
    Some(lines.join("\n"))
}

/// Append the note to the session, if anything is unanswered. Best
/// effort: a failure here must never fail the turn.
pub fn annotate(db: &Db, sessions: &SessionStore, session: &str, ids: &[String], now: &Timestamp) {
    let actions = collect_unanswered(db, ids);
    let Some(note) = render_note(&actions) else {
        return;
    };
    let _ = sessions.append(session, &ChatMessage::assistant(&note), Trust::System, now);
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use hatch_policy::{ActionClass, NewApproval};
    use serde_json::json;

    fn approval(key: &str, title: &str) -> NewApproval {
        NewApproval {
            idempotency_key: key.into(),
            kind: "send_email".into(),
            class: ActionClass::SendMessage,
            title: title.into(),
            preview: title.into(),
            payload: json!({}),
            reasons: vec![],
            origin: "chat:s1".into(),
            ttl: Duration::hours(72),
        }
    }

    fn submit(db: &Db, key: &str, title: &str) -> String {
        let now = hatch_core::time::now();
        match ApprovalQueue::new(db)
            .submit(&approval(key, title), &now)
            .unwrap()
        {
            hatch_policy::SubmitOutcome::Created(id) => id,
            hatch_policy::SubmitOutcome::Existing(id, _) => id,
        }
    }

    #[test]
    fn pending_is_listed_decided_is_not() {
        let db = Db::open_in_memory().unwrap();
        let now = hatch_core::time::now();
        let p = submit(&db, "k1", "发邮件给老板");
        let a = submit(&db, "k2", "发邮件给客户");
        let r = submit(&db, "k3", "发邮件给同事");
        ApprovalQueue::new(&db)
            .decide(&a, true, "ok", &now)
            .unwrap();
        ApprovalQueue::new(&db)
            .decide(&r, false, "no", &now)
            .unwrap();
        let got = collect_unanswered(
            &db,
            &[p.clone(), a.clone(), r.clone(), p.clone(), "missing".into()],
        );
        assert_eq!(
            got,
            vec![UnansweredAction {
                id: p,
                kind: "send_email".into(),
                title: "发邮件给老板".into()
            }]
        );
    }

    #[test]
    fn render_note_names_actions_and_caps_the_list() {
        assert_eq!(render_note(&[]), None);
        let one = vec![UnansweredAction {
            id: "a".into(),
            kind: "send_email".into(),
            title: "发邮件给老板".into(),
        }];
        let note = render_note(&one).unwrap();
        assert!(note.contains("send_email"), "{note}");
        assert!(note.contains("发邮件给老板"), "{note}");
        assert!(note.contains("不要向用户汇报成已完成"), "{note}");
        let many: Vec<UnansweredAction> = (0..12)
            .map(|i| UnansweredAction {
                id: format!("a{i}"),
                kind: format!("tool_{i}"),
                title: format!("动作 {i}"),
            })
            .collect();
        let note = render_note(&many).unwrap();
        assert!(note.contains("tool_9"), "{note}");
        assert!(!note.contains("tool_10"), "{note}");
        assert!(note.contains("等 2 项"), "{note}");
    }
}
