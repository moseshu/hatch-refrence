//! Unit tests for `task_command_edit`: target resolution,
//! ambiguity, staleness and executed-step refusal.

#[cfg(test)]
mod tests {
    use crate::commands::task_command_edit::*;
    use hatch_core::time::{parse_rfc3339, Timestamp};
    use hatch_store::Db;
    use hatch_tasks::{ExecStatus, StepSpec, TaskStore, UnitStore};
    use std::sync::Arc;

    fn setup() -> (Arc<Db>, String, Timestamp) {
        let db = Arc::new(Db::open_in_memory().unwrap());
        let now = parse_rfc3339("2026-11-15T10:00:00Z").unwrap();
        let task = TaskStore::new(&db)
            .create("meeting-invite", "邀请", "drill", "", 3, None, &now)
            .unwrap();
        let units = UnitStore::new(&db);
        let lease = TaskStore::new(&db)
            .start(&task.id, "drill", 300, &now)
            .unwrap()
            .lease()
            .unwrap();
        // Three steps in one call (a second `ensure_steps` would
        // re-use idx 0 and silently skip the insert): two pending
        // with overlapping text (both mention 李婷), one done (also
        // mentions 李婷) that must never match.
        units
            .ensure_steps(
                &task.id,
                &lease,
                &[
                    StepSpec {
                        step_id: "read-mail".into(),
                        title: "读李婷的邮件".into(),
                        tool: "mail_search".into(),
                        arguments: serde_json::json!({"query": "李婷"}),
                        expected: "找到决定".into(),
                    },
                    StepSpec {
                        step_id: "send".into(),
                        title: "发邀请给李婷".into(),
                        tool: "propose_email".into(),
                        arguments: serde_json::json!({"to": "李婷 <li.ting@corp.example>"}),
                        expected: "邀请已发".into(),
                    },
                    StepSpec {
                        step_id: "read-cal".into(),
                        title: "查李婷的日程".into(),
                        tool: "calendar_list".into(),
                        arguments: serde_json::json!({"query": "李婷"}),
                        expected: "有空档".into(),
                    },
                ],
                &now,
            )
            .unwrap();
        units
            .complete_step(&task.id, &lease, "read-mail", ExecStatus::Done, "已读", &now)
            .unwrap();
        (db, task.id, now)
    }

    #[test]
    fn structured_edit_resolves_by_id_title_or_unique_keyword() {
        let (db, task, _now) = setup();
        // By id.
        let e = CommandEdit::from_json(
            &serde_json::json!({"step": "send", "expected": "邀请已发给王宇"}),
        )
        .unwrap();
        let CommandResolution::Edit(r) = resolve_command_edit(&db, &task, "", Some(&e)).unwrap()
        else {
            panic!("expected a resolved edit");
        };
        assert_eq!(r.step_id, "send");
        assert_eq!(r.edit.expected.as_deref(), Some("邀请已发给王宇"));
        // By exact title.
        let e = CommandEdit::from_json(
            &serde_json::json!({"step": "查李婷的日程", "expected": "改"}),
        )
        .unwrap();
        let CommandResolution::Edit(r) = resolve_command_edit(&db, &task, "", Some(&e)).unwrap()
        else {
            panic!("expected a resolved edit");
        };
        assert_eq!(r.step_id, "read-cal");
        // By unique keyword (only the send step proposes mail).
        let e = CommandEdit::from_json(
            &serde_json::json!({"step": "propose_email", "expected": "改"}),
        )
        .unwrap();
        let CommandResolution::Edit(r) = resolve_command_edit(&db, &task, "", Some(&e)).unwrap()
        else {
            panic!("expected a resolved edit");
        };
        assert_eq!(r.step_id, "send");
    }

    #[test]
    fn ambiguous_or_missing_targets_block_with_a_reason() {
        let (db, task, _now) = setup();
        // "李婷" matches two pending steps (send + read-cal).
        let e = CommandEdit::from_json(&serde_json::json!({"step": "李婷", "expected": "改"})).unwrap();
        let err = resolve_command_edit(&db, &task, "", Some(&e)).unwrap_err();
        assert!(err.to_string().contains("2 个未执行步骤"), "{err}");
        // Nothing matches.
        let e = CommandEdit::from_json(&serde_json::json!({"step": "不存在的步骤", "expected": "改"})).unwrap();
        let err = resolve_command_edit(&db, &task, "", Some(&e)).unwrap_err();
        assert!(err.to_string().contains("没有找到"), "{err}");
        // The done step (read-mail) must never resolve, even by id.
        let e = CommandEdit::from_json(&serde_json::json!({"step": "read-mail", "expected": "改"})).unwrap();
        let err = resolve_command_edit(&db, &task, "", Some(&e)).unwrap_err();
        assert!(err.to_string().contains("没有找到"), "{err}");
    }

    #[test]
    fn free_text_replacement_needs_a_unique_unexecuted_step() {
        let (db, task, _now) = setup();
        // "李婷" is ambiguous across the two pending steps.
        let err = resolve_command_edit(&db, &task, "把李婷改成王宇", None).unwrap_err();
        assert!(err.to_string().contains("2 个未执行步骤"), "{err}");
        // "发邀请给李婷" is unique to the send step's title.
        let CommandResolution::Edit(r) =
            resolve_command_edit(&db, &task, "把发邀请给李婷改成发邀请给王宇", None).unwrap()
        else {
            panic!("expected a resolved edit");
        };
        assert_eq!(r.step_id, "send");
        assert_eq!(r.edit.title.as_deref(), Some("发邀请给王宇"));
        // No replacement pattern: no edit intent.
        assert!(matches!(
            resolve_command_edit(&db, &task, "第二个候选也加进去", None).unwrap(),
            CommandResolution::RecordOnly(None)
        ));
        // A replacement that matches no step: recorded only, with a
        // note (the instruction may target the task itself).
        assert!(matches!(
            resolve_command_edit(&db, &task, "把标题改成周报", None).unwrap(),
            CommandResolution::RecordOnly(Some(_))
        ));
    }

    #[test]
    fn apply_edits_and_refuses_stale_or_executed_steps() {
        let (db, task, now) = setup();
        // A clear structured edit applies through edit_step: version
        // 1 → 2, and the step's expected changes.
        let e = CommandEdit::from_json(
            &serde_json::json!({"step": "send", "expected": "邀请已发给王宇"}),
        )
        .unwrap();
        let msg = apply_command_edit(&db, &task, 1, "把收件人改成王宇", Some(&e), "cli", &now).unwrap();
        assert!(msg.contains("v1 → v2"), "{msg}");
        assert_eq!(
            UnitStore::new(&db).get_step(&task, "send").unwrap().unwrap().expected,
            "邀请已发给王宇"
        );
        // The same command with the old version is now stale.
        let e = CommandEdit::from_json(
            &serde_json::json!({"step": "send", "expected": "再改"}),
        )
        .unwrap();
        let err = apply_command_edit(&db, &task, 1, "再改", Some(&e), "cli", &now).unwrap_err();
        assert!(err.to_string().contains("v2"), "{err}");
        // Editing the done step is refused even at the fresh version.
        let e = CommandEdit::from_json(
            &serde_json::json!({"step": "read-mail", "expected": "改"}),
        )
        .unwrap();
        let err = apply_command_edit(&db, &task, 2, "改已读步骤", Some(&e), "cli", &now).unwrap_err();
        assert!(err.to_string().contains("没有找到"), "{err}");
    }
}
