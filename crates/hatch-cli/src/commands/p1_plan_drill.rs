//! P1 plan drill: a submitted plan draft runs through the generic
//! Plan-and-Execute runner — plan approval, then each `fs_write`
//! step through its own approval, then Done. The plan is a draft
//! the user submitted in chat, not a fixture: the steps and their
//! contents come from the submit args only.

use super::p1_submit_drill::{drive_to_done, only_task, run_chat_submit, setup};
use crate::commands::task_resident::ResidentWorker;
use hatch_tasks::TaskStore;
use serde_json::json;

#[test]
fn chat_submit_plan_runs_each_step_through_approval() {
    let (app, _home) = setup();
    let report = run_chat_submit(
        &app,
        "把两句话分别写进两个文件：第一句写进 a.txt，第二句写进 b.txt",
        json!({
            "goal": "把两句话分别写进两个文件",
            "steps": [
                {"id": "w1", "title": "写第一句", "tool": "fs_write",
                 "arguments": {"path": "a.txt", "content": "第一句"}, "expected": "a.txt 已写入"},
                {"id": "w2", "title": "写第二句", "tool": "fs_write",
                 "arguments": {"path": "b.txt", "content": "第二句"}, "expected": "b.txt 已写入"}
            ],
        }),
    );
    assert_eq!(report.stop, hatch_agent::StopReason::Answered);
    let task_id = only_task(&app);
    let task = TaskStore::new(&app.db).get(&task_id).unwrap().unwrap();
    assert_eq!(task.kind, "plan");
    let mut worker = ResidentWorker::new(&app);
    assert!(
        drive_to_done(&app, &mut worker, &task_id),
        "计划任务应在审批流转后完成"
    );
    let out_dir = app.home.join("task-out").join(&task_id);
    assert_eq!(
        std::fs::read_to_string(out_dir.join("a.txt")).unwrap(),
        "第一句"
    );
    assert_eq!(
        std::fs::read_to_string(out_dir.join("b.txt")).unwrap(),
        "第二句"
    );
}
