//! P1 D1 fields: the durable `goal`, milestones as a JSON column, and
//! the `current_step` pointer, persisted with the plan (public API only).

use hatch_core::time::now;
use hatch_plan::{Milestone, PlanStore, PlanStep, RiskLevel, StepStatus};
use hatch_store::Db;
use serde_json::json;

fn step(id: &str) -> PlanStep {
    PlanStep {
        id: id.into(),
        title: "t".into(),
        tool: "memory_search".into(),
        arguments: json!({"query": "x"}),
        files: vec![],
        expected: "ok".into(),
        risk: RiskLevel::Low,
        status: StepStatus::Pending,
        output: String::new(),
        approval_id: None,
    }
}

#[test]
fn create_defaults_goal_milestones_current_step() {
    let db = Db::open_in_memory().unwrap();
    let store = PlanStore::new(&db);
    let now = now();
    // Goal defaults to empty; milestones to none; no current step.
    let plan = store
        .create("做 X", "", "m", vec![step("s1")], false, &now)
        .unwrap();
    assert_eq!(plan.goal, "");
    assert!(plan.milestones.is_empty());
    assert_eq!(plan.current_step, None);
    // And they round-trip through a fresh read.
    let plan = store.get(&plan.id).unwrap().unwrap();
    assert_eq!(plan.goal, "");
    assert!(plan.milestones.is_empty());
    assert_eq!(plan.current_step, None);
}

#[test]
fn goal_set_at_create_and_changed_later() {
    let db = Db::open_in_memory().unwrap();
    let store = PlanStore::new(&db);
    let now = now();
    let plan = store
        .create("写周报", "每周准时发出周报", "m", vec![step("s1")], true, &now)
        .unwrap();
    assert_eq!(plan.goal, "每周准时发出周报");
    assert_eq!(plan.task, "写周报", "goal stays apart from task");

    // Changing the goal never touches the status.
    let plan = store.set_goal(&plan.id, "周报自动化", &now).unwrap();
    assert_eq!(plan.goal, "周报自动化");
    assert_eq!(plan.task, "写周报");
    let plan = store.get(&plan.id).unwrap().unwrap();
    assert_eq!(plan.goal, "周报自动化");
    assert!(store.set_goal("no-such-plan", "x", &now).is_err());
}

#[test]
fn milestones_add_mark_done_and_replace() {
    let db = Db::open_in_memory().unwrap();
    let store = PlanStore::new(&db);
    let now = now();
    let plan = store
        .create("做 X", "", "m", vec![step("s1")], true, &now)
        .unwrap();

    let plan = store.upsert_milestone(&plan.id, "初稿", false, &now).unwrap();
    assert_eq!(
        plan.milestones,
        vec![Milestone::new("初稿")],
        "added as not done"
    );
    let plan = store
        .upsert_milestone(&plan.id, "初稿", true, &now)
        .unwrap();
    assert!(plan.milestones[0].done, "same title flips the flag");
    let plan = store.upsert_milestone(&plan.id, "终稿", false, &now).unwrap();
    assert_eq!(plan.milestones.len(), 2);

    // Whole-list replace, then a fresh read round-trips it.
    let plan = store
        .set_milestones(
            &plan.id,
            &[Milestone {
                title: "发布".into(),
                done: true,
            }],
            &now,
        )
        .unwrap();
    assert_eq!(plan.milestones.len(), 1);
    let plan = store.get(&plan.id).unwrap().unwrap();
    assert_eq!(plan.milestones[0].title, "发布");
    assert!(plan.milestones[0].done);

    assert!(store.upsert_milestone("no-such-plan", "x", false, &now).is_err());
    assert!(store.set_milestones("no-such-plan", &[], &now).is_err());
}
