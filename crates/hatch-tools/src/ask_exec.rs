//! Executor for `ask_user` approvals.
//!
//! Answering a question has no side effect to perform — the answer
//! already travelled in the approval's `note`. This executor exists
//! so the generic approve path (CLI/API "approve" on the queue row,
//! worker recovery) settles the row cleanly instead of failing it
//! with "no executor configured": it echoes the note back as the
//! receipt, which is exactly what the waiting `ask_user` tool call
//! reads as the answer.

use crate::executors::{ActionExecutor, ExecContext};
use crate::verification::EffectContract;
use hatch_core::Result;
use hatch_policy::ApprovalQueue;
use serde_json::Value;

pub struct AskUserExecutor;

impl ActionExecutor for AskUserExecutor {
    fn kind(&self) -> &'static str {
        crate::builtin::ask_user::ASK_KIND
    }

    fn execute(&self, _payload: &Value, ctx: &ExecContext) -> Result<String> {
        let a = ApprovalQueue::new(ctx.db).get(ctx.approval_id)?;
        if a.note.trim().is_empty() {
            Ok("用户已确认，未给出具体选项".into())
        } else {
            Ok(a.note)
        }
    }

    fn contract(&self) -> EffectContract {
        EffectContract::ReadOnly
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::ask_user::{answer_question, Choice};
    use hatch_core::time::now as now_ts;
    use hatch_policy::{ActionClass, NewApproval, SubmitOutcome};
    use hatch_store::Db;
    use serde_json::json;

    #[test]
    fn executor_echoes_the_answer_note() {
        let db = Db::open_in_memory().unwrap();
        let now = now_ts();
        let q = ApprovalQueue::new(&db);
        let id = match q
            .submit(
                &NewApproval {
                    turn_id: String::new(),
                    idempotency_key: "ask-exec-test".into(),
                    kind: crate::builtin::ask_user::ASK_KIND.into(),
                    class: ActionClass::ReadPersonal,
                    title: "问题：选哪个".into(),
                    preview: "问题: 选哪个".into(),
                    payload: json!({"question": "选哪个", "options": [
                        {"label": "甲"}, {"label": "乙"}
                    ], "allow_other": true}),
                    reasons: vec![],
                    origin: "test".into(),
                    ttl: chrono::Duration::minutes(5),
                },
                &now,
            )
            .unwrap()
        {
            SubmitOutcome::Created(id) => id,
            SubmitOutcome::Existing(id, _) => id,
        };
        answer_question(&db, &id, Some(Choice::Index(0)), None, &now).unwrap();
        let ctx = ExecContext {
            db: &db,
            now,
            approval_id: &id,
            origin: "test",
            cancel: None,
        };
        let receipt = AskUserExecutor.execute(&json!({}), &ctx).unwrap();
        let v: Value = serde_json::from_str(&receipt).unwrap();
        assert_eq!(v["label"], "甲");
    }
}
