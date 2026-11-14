//! Drills for task 9: a regression can be located to its stage
//! and class; an old checkpoint restores in the new version; the
//! feature flag has two correct states and never hides in-flight
//! tasks or receipts; a canary rolls out, kills, and rolls back
//! with the original receipts still queryable.
//!
//! Everything runs on the scripted tools / test doubles and
//! fixture data: no live model, no real mail, no real account.

#![cfg(test)]

use super::release_drill_helpers::{approve_plan, now, run_deps, step, FailTool, OkTool};
use super::task::TaskCmd;
use crate::commands::task;
use hatch_plan::{run_plan, FailureKind, PlanStore, StepVerifier, StepVerify};
use hatch_policy::{Gatekeeper, PolicyConfig};
use hatch_release::{
    all_passed, assign, check_checkpoint_compat, policy_note, verify_release, CheckpointCompat,
    FailureClass, FailureEvidence, FlagStore, ReleaseStore, RolloutStore, SampleStore, Stage,
};
use hatch_snapshots::SnapshotStore;
use hatch_store::{Db, SCHEMA_VERSION};
use hatch_tasks::{TaskStore, UnitCallSpec, UnitStore};
use hatch_tools::builtin::default_registry;
use hatch_tools::ExecutorRegistry;
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;

/// (a) A regression can be located: the failed step's stage and
/// attribution class, recorded as a sample under that class.
#[test]
fn regression_locates_stage_and_class() {
    let db = Db::open_in_memory().unwrap();
    let exec = ExecutorRegistry::new();
    let gate = Gatekeeper::new(PolicyConfig::default());

    // Case 1: a tool that fails at call time — the call cannot
    // succeed as written, at the tool layer.
    let mut reg1 = default_registry();
    reg1.register(Arc::new(FailTool));
    let plan = PlanStore::new(&db)
        .create("test", "", "", vec![step("s1", "fail_read")], false, &now())
        .unwrap();
    approve_plan(&db, &plan.id);
    let report = run_plan(&run_deps(&db, &reg1, &exec, &gate, None), &plan.id, &now()).unwrap();
    assert_eq!(report.steps[0].failure_kind, Some(FailureKind::Permanent));
    let ev = FailureEvidence {
        tool: Some("fail_read".into()),
        error: "读取失败：文件不存在".into(),
        failure_kind: Some("permanent".into()),
        tool_fault: true,
        ..Default::default()
    };
    assert_eq!(
        hatch_release::classify(&ev),
        (FailureClass::Tool, Stage::Execute)
    );

    // Case 2: the tool ran, but the delivery does not match the
    // acceptance — a delivery fault at the verify stage.
    let mut reg2 = default_registry();
    reg2.register(Arc::new(OkTool));
    let plan = PlanStore::new(&db)
        .create("test", "", "", vec![step("s2", "ok_read")], false, &now())
        .unwrap();
    approve_plan(&db, &plan.id);
    let fail_all: StepVerifier = &|_step, _out| StepVerify::Fail("与预期不符".into());
    let report = run_plan(
        &run_deps(&db, &reg2, &exec, &gate, Some(fail_all)),
        &plan.id,
        &now(),
    )
    .unwrap();
    assert_eq!(report.steps[0].failure_kind, Some(FailureKind::Permanent));
    let ev = FailureEvidence {
        tool: Some("ok_read".into()),
        error: "与预期不符".into(),
        failure_kind: Some("permanent".into()),
        receipt_mismatch: true,
        ..Default::default()
    };
    assert_eq!(
        hatch_release::classify(&ev),
        (FailureClass::Delivery, Stage::Verify)
    );

    // Both regressions become samples, kept by class.
    let samples = SampleStore::new(&db);
    samples
        .record(
            "meeting-brief",
            FailureClass::Tool,
            Stage::Execute,
            "tsk_case1",
            "s1",
            "工具 fail_read 调用失败（已脱敏）",
            true,
            &now(),
        )
        .unwrap();
    samples
        .record(
            "meeting-brief",
            FailureClass::Delivery,
            Stage::Verify,
            "tsk_case2",
            "s2",
            "交付与预期不符（已脱敏）",
            true,
            &now(),
        )
        .unwrap();
    assert_eq!(samples.list_by_class(FailureClass::Tool).unwrap().len(), 1);
    assert_eq!(
        samples.list_by_class(FailureClass::Delivery).unwrap().len(),
        1
    );
    let counts = samples.count_by_class().unwrap();
    assert_eq!(
        counts
            .iter()
            .find(|(c, _)| *c == FailureClass::Delivery)
            .unwrap()
            .1,
        1
    );
}

/// (b) An old checkpoint restores in the new version; a future
/// checkpoint blocks the release.
#[test]
fn old_checkpoint_restores_in_new_version() {
    let home = TempDir::new().unwrap();
    let snap_dir = home.path().join("snapshots");
    let store = SnapshotStore::new(&snap_dir);
    let file = home.path().join("note.txt");
    std::fs::write(&file, b"v1 content").unwrap();
    let manifest = store.create(std::slice::from_ref(&file), "test", &now()).unwrap();
    // New snapshots are stamped with the current versions.
    assert_eq!(manifest.schema_version, Some(SCHEMA_VERSION));
    assert_eq!(manifest.policy_version.as_deref(), Some("1"));

    // Simulate an old checkpoint: rewrite the manifest to schema
    // 17 / policy 0.
    let manifest_path = snap_dir.join(&manifest.id).join("manifest.json");
    let mut raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
    raw["schema_version"] = json!(17);
    raw["policy_version"] = json!("0");
    std::fs::write(&manifest_path, serde_json::to_string_pretty(&raw).unwrap()).unwrap();
    let old = store.get(&manifest.id).unwrap();
    assert_eq!(old.schema_version, Some(17));
    assert_eq!(
        check_checkpoint_compat(old.schema_version, SCHEMA_VERSION),
        CheckpointCompat::OlderRestorable {
            from_schema: Some(17)
        }
    );
    assert!(policy_note(old.policy_version.as_deref(), "1").is_some());

    // The old checkpoint still restores: the files are plain copies.
    std::fs::write(&file, b"changed since").unwrap();
    let report = store.restore(&manifest.id, &now()).unwrap();
    assert_eq!(report.restored, vec![file.display().to_string()]);
    assert_eq!(std::fs::read(&file).unwrap(), b"v1 content");

    // A checkpoint from the future blocks a release.
    raw["schema_version"] = json!(99);
    std::fs::write(&manifest_path, serde_json::to_string_pretty(&raw).unwrap()).unwrap();
    let future = store.get(&manifest.id).unwrap();
    assert!(matches!(
        check_checkpoint_compat(future.schema_version, SCHEMA_VERSION),
        CheckpointCompat::NewerBlocked { .. }
    ));
    let db = Db::open_in_memory().unwrap();
    let rel = ReleaseStore::new(&db)
        .create("hash", &Default::default(), "1", "v1", &now())
        .unwrap();
    let checks = verify_release(
        &db,
        &rel.id,
        &[(
            future.id.clone(),
            future.schema_version,
            future.policy_version.clone(),
        )],
        SCHEMA_VERSION,
        "1",
    )
    .unwrap();
    assert!(!all_passed(&checks));
    assert!(checks
        .iter()
        .any(|c| c.name == "旧检查点可恢复" && !c.passed));

    // A pre-versioning manifest (no version fields at all) is an
    // older restorable checkpoint, not an error.
    raw.as_object_mut().unwrap().remove("schema_version");
    raw.as_object_mut().unwrap().remove("policy_version");
    std::fs::write(&manifest_path, serde_json::to_string_pretty(&raw).unwrap()).unwrap();
    let legacy = store.get(&manifest.id).unwrap();
    assert_eq!(legacy.schema_version, None);
    assert_eq!(
        check_checkpoint_compat(legacy.schema_version, SCHEMA_VERSION),
        CheckpointCompat::OlderRestorable { from_schema: None }
    );
}

/// (c) The flag has two correct states: closed, creation is
/// refused but in-flight tasks and issued receipts stay
/// queryable; open, creation works again.
#[test]
fn flag_two_states_keep_in_flight_tasks_and_receipts() {
    let home = TempDir::new().unwrap();
    let home_path = home.path();
    // One in-flight task with an issued receipt, created while
    // the entry is open.
    task::run(
        Some(home_path),
        TaskCmd::Create {
            title: "在途任务".into(),
            kind: None,
            max_model_calls: 0,
        },
    )
    .unwrap();
    let db = Db::open(&home_path.join("hatch.db")).unwrap();
    let task_id: String = db
        .conn()
        .query_row("SELECT id FROM tasks LIMIT 1", [], |r| r.get(0))
        .unwrap();
    let store = TaskStore::new(&db);
    let claimed = store.start(&task_id, "worker", 60, &now()).unwrap();
    let units = UnitStore::new(&db);
    let unit = units
        .begin_unit(
            &task_id,
            &claimed.lease().unwrap(),
            "triage",
            &[UnitCallSpec {
                id: "call_1".into(),
                tool: "mail_search".into(),
                arguments: json!({}),
            }],
            &now(),
        )
        .unwrap();

    // Close the entry.
    FlagStore::new(&db)
        .set(hatch_release::NEW_TASK_ENTRY, false, &now())
        .unwrap();
    let err = task::run(
        Some(home_path),
        TaskCmd::Create {
            title: "新任务".into(),
            kind: None,
            max_model_calls: 0,
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("新任务入口已关闭"));
    // The in-flight task is still readable, and its receipt is
    // still queryable.
    task::run(
        Some(home_path),
        TaskCmd::Show {
            id: task_id.clone(),
        },
    )
    .unwrap();
    let again = units.latest_unit(&task_id, "triage").unwrap().unwrap();
    assert_eq!(again.unit_seq, unit.unit_seq);

    // Reopen: creation works again.
    FlagStore::new(&db)
        .set(hatch_release::NEW_TASK_ENTRY, true, &now())
        .unwrap();
    task::run(
        Some(home_path),
        TaskCmd::Create {
            title: "新任务".into(),
            kind: None,
            max_model_calls: 0,
        },
    )
    .unwrap();
}

/// (d) Canary rollout, kill switch, and rollback: the receipts
/// recorded under the rolled-back release stay queryable.
#[test]
fn canary_kill_and_rollback_keeps_receipts() {
    let db = Db::open_in_memory().unwrap();
    let releases = ReleaseStore::new(&db);
    let a = releases
        .create("hash-a", &Default::default(), "1", "v1", &now())
        .unwrap();
    releases.activate(&a.id).unwrap();
    let b = releases
        .create("hash-b", &Default::default(), "1", "v2", &now())
        .unwrap();
    let rollouts = RolloutStore::new(&db);
    let rollout = rollouts.create(&b.id, &[10, 50, 100], &now()).unwrap();
    assert_eq!(rollout.current_percent(), 10);

    // At 10% the ids split both ways, and the same id always
    // lands in the same bucket (sticky without stored state).
    let mut inside = String::new();
    let mut outside = String::new();
    for i in 0..200 {
        let id = format!("tsk_{i}");
        if assign(&id, &rollout) {
            if inside.is_empty() {
                inside = id;
            }
        } else if outside.is_empty() {
            outside = id;
        }
    }
    assert!(!inside.is_empty() && !outside.is_empty());
    assert_eq!(assign(&inside, &rollout), assign(&inside, &rollout));
    // Advancing is monotonic: the 10% member stays in at 50%,
    // and at 100% everything is in.
    let rollout = rollouts.advance(&b.id).unwrap();
    assert_eq!(rollout.current_percent(), 50);
    assert!(assign(&inside, &rollout));
    let rollout = rollouts.advance(&b.id).unwrap();
    assert_eq!(rollout.current_percent(), 100);
    assert!(assign(&outside, &rollout));
    assert!(rollouts.advance(&b.id).is_err());

    // Kill switch: no new canary assignments.
    let rollout = rollouts.kill(&b.id).unwrap();
    assert_eq!(rollout.current_percent(), 0);
    assert!(!assign(&inside, &rollout));
    assert!(!assign(&outside, &rollout));

    // A receipt recorded while v2 was the candidate release.
    let store = TaskStore::new(&db);
    let task = store
        .create("meeting-brief", "简报", "cli", "", 3, None, &now())
        .unwrap();
    let claimed = store.start(&task.id, "worker", 60, &now()).unwrap();
    let units = UnitStore::new(&db);
    let unit = units
        .begin_unit(
            &task.id,
            &claimed.lease().unwrap(),
            "triage",
            &[UnitCallSpec {
                id: "call_1".into(),
                tool: "mail_search".into(),
                arguments: json!({}),
            }],
            &now(),
        )
        .unwrap();

    // Roll back to v1: the active release changes, the receipt
    // does not move and stays queryable.
    releases.activate(&a.id).unwrap();
    assert_eq!(releases.active().unwrap().unwrap().id, a.id);
    let again = units.latest_unit(&task.id, "triage").unwrap().unwrap();
    assert_eq!(again.unit_seq, unit.unit_seq);
    // The ledger still counts the unit (verify's receipt check).
    let count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM task_units", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
}
