//! Acceptance drills for the P0 approval unification (plan-3 item 1):
//! one task-bound execution service shared by the CLI, the HTTP API
//! and the worker recovery path. Every drill runs on the scripted
//! model and test-double senders — no real account, no real mail, no
//! `--live`.
//!
//! (a) stale plan version — refused, approval invalidated, nothing sent;
//! (b) cancelled task — refused (including a late-linked approval),
//!     nothing sent;
//! (c) concurrent approve — exactly one execution;
//! (d) sender selection — a Gmail task uses the Gmail sender, a
//!     fixture task lands in the outbox (`--dry-run` forces the
//!     outbox even for a Gmail task);
//! (e) receipt lost — the send happened once, the approval needs
//!     human review, and no later approve/recovery resends;
//! (f) definite refusal vs unknown outcome — different branches,
//!     only the definite failure is retryable;
//! (g) worker resume — a decision persisted before a crash is
//!     executed by the resumed worker through the same service.

#![cfg(test)]

use super::approval_service::{ApprovalService, ServiceOutcome};
use super::approval_service_drill_helpers::*;
use super::task_control::cancel_task;
use super::task_worker::{run_worker, KillPoint, WorkerOpts};
use hatch_connectors::MailSender;
use hatch_core::time::parse_rfc3339;
use hatch_eval::World;
use hatch_policy::{ApprovalQueue, ApprovalStatus};
use hatch_tasks::{EventLog, TaskStatus, TaskStore, UnitStore};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use tempfile::TempDir;

fn world_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../eval/worlds/meeting-brief")
}

#[test]
fn stale_version_approval_is_refused_and_invalidated() {
    let (db, app, _home, now) = setup();
    let sent = Arc::new(AtomicUsize::new(0));
    let task = make_task(&db, "meeting-brief", "fixture", &now);
    let id = email_approval(&db, "p0-a", &now);
    link(&db, &task, &id, &now);
    // The plan moved on after the approval was bound.
    TaskStore::new(&db).bump_plan_version(&task, 1, &now).unwrap();
    let outcome = ApprovalService::with_sender(
        &app,
        Arc::new(CountingSender {
            sent: Arc::clone(&sent),
        }),
    )
    .approve_and_execute(&id, "", false, &now)
    .unwrap();
    assert!(
        matches!(outcome, ServiceOutcome::Refused(_)),
        "stale approval must be refused, got {outcome:?}"
    );
    assert_eq!(
        ApprovalQueue::new(&db).get(&id).unwrap().status,
        ApprovalStatus::Invalidated,
        "a refused stale approval is invalidated so no other path can run it"
    );
    assert_eq!(sent.load(Ordering::SeqCst), 0, "nothing may execute");
    assert_eq!(outbox_files(&app), 0);
}

#[test]
fn cancelled_task_approval_is_refused() {
    let (db, app, _home, now) = setup();
    let sent = Arc::new(AtomicUsize::new(0));
    let task = make_task(&db, "meeting-invite", "gmail", &now);
    let before = email_approval(&db, "p0-b1", &now);
    link(&db, &task, &before, &now);
    cancel_task(&db, &task, "drill cancel", &now).unwrap();
    // A late-linked approval (linked after the cancel, e.g. a race
    // with the cancellation) must be refused by the service guard
    // itself, not only by the cancel path.
    let late = email_approval(&db, "p0-b2", &now);
    link(&db, &task, &late, &now);
    let service = ApprovalService::with_sender(
        &app,
        Arc::new(CountingSender {
            sent: Arc::clone(&sent),
        }),
    );
    for id in [&before, &late] {
        let outcome = service.approve_and_execute(id, "", false, &now).unwrap();
        assert!(
            matches!(outcome, ServiceOutcome::Refused(_)),
            "approval on a cancelled task must be refused, got {outcome:?}"
        );
        assert_eq!(
            ApprovalQueue::new(&db).get(id).unwrap().status,
            ApprovalStatus::Invalidated
        );
    }
    assert_eq!(sent.load(Ordering::SeqCst), 0, "nothing may execute");
}

#[test]
fn concurrent_approve_executes_once() {
    let (db, app, _home, now) = setup();
    let task = make_task(&db, "meeting-invite", "gmail", &now);
    let id = email_approval(&db, "p0-c", &now);
    link(&db, &task, &id, &now);
    let sent = Arc::new(AtomicUsize::new(0));
    let started = Arc::new((Mutex::new(false), Condvar::new()));
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let sender = Arc::new(BlockingSender {
        sent: Arc::clone(&sent),
        started: Arc::clone(&started),
        gate: Arc::clone(&gate),
    });
    let (first, second) = std::thread::scope(|s| {
        let h1 = s.spawn(|| {
            ApprovalService::with_sender(&app, Arc::clone(&sender) as Arc<dyn MailSender>)
                .approve_and_execute(&id, "", false, &now)
                .unwrap()
        });
        // Wait until the first attempt is inside the send (decided,
        // ledger entry open) before staging the second one.
        {
            let (lock, cvar) = &*started;
            let mut guard = lock.lock().unwrap();
            while !*guard {
                guard = cvar
                    .wait_timeout(guard, Duration::from_secs(10))
                    .unwrap()
                    .0;
            }
            assert!(*guard, "first approve did not reach the sender");
        }
        let h2 = s.spawn(|| {
            ApprovalService::with_sender(&app, Arc::clone(&sender) as Arc<dyn MailSender>)
                .approve_and_execute(&id, "", false, &now)
                .unwrap()
        });
        // The second request finds the approval already approved and
        // is refused — it never reaches the executor.
        let second = h2.join().unwrap();
        {
            let (lock, cvar) = &*gate;
            *lock.lock().unwrap() = true;
            cvar.notify_all();
        }
        (h1.join().unwrap(), second)
    });
    assert!(
        matches!(first, ServiceOutcome::Executed(_)),
        "first approve executes, got {first:?}"
    );
    assert!(
        matches!(second, ServiceOutcome::Refused(_)),
        "concurrent second approve is refused, got {second:?}"
    );
    assert_eq!(sent.load(Ordering::SeqCst), 1, "exactly one send");
}

#[test]
fn sender_selection_follows_task_source() {
    let (db, app, _home, now) = setup();
    let sent = Arc::new(AtomicUsize::new(0));
    let stub = Arc::new(CountingSender {
        sent: Arc::clone(&sent),
    });
    // Gmail task: the stub (standing in for the Gmail sender) is used.
    let gmail_task = make_task(&db, "meeting-invite", "gmail", &now);
    let gmail_id = email_approval(&db, "p0-d1", &now);
    link(&db, &gmail_task, &gmail_id, &now);
    let outcome = ApprovalService::with_sender(&app, Arc::clone(&stub) as Arc<dyn MailSender>)
        .approve_and_execute(&gmail_id, "", false, &now)
        .unwrap();
    assert!(matches!(outcome, ServiceOutcome::Executed(_)), "{outcome:?}");
    assert_eq!(sent.load(Ordering::SeqCst), 1);
    assert_eq!(outbox_files(&app), 0, "Gmail send must not land in the outbox");
    // Fixture task: the stub is untouched, the mail lands in the outbox.
    let fixture_task = make_task(&db, "meeting-brief", "fixture", &now);
    let fixture_id = email_approval(&db, "p0-d2", &now);
    link(&db, &fixture_task, &fixture_id, &now);
    let outcome = ApprovalService::with_sender(&app, Arc::clone(&stub) as Arc<dyn MailSender>)
        .approve_and_execute(&fixture_id, "", false, &now)
        .unwrap();
    assert!(matches!(outcome, ServiceOutcome::Executed(_)), "{outcome:?}");
    assert_eq!(sent.load(Ordering::SeqCst), 1, "fixture task must not use the Gmail sender");
    assert_eq!(outbox_files(&app), 1, "fixture send lands in the outbox");
    // Gmail task with --dry-run: the outbox wins even for Gmail.
    let dry_task = make_task(&db, "meeting-invite", "gmail", &now);
    let dry_id = email_approval(&db, "p0-d3", &now);
    link(&db, &dry_task, &dry_id, &now);
    let outcome = ApprovalService::with_sender(&app, Arc::clone(&stub) as Arc<dyn MailSender>)
        .approve_and_execute(&dry_id, "", true, &now)
        .unwrap();
    assert!(matches!(outcome, ServiceOutcome::Executed(_)), "{outcome:?}");
    assert_eq!(sent.load(Ordering::SeqCst), 1, "dry run must never send for real");
    assert_eq!(outbox_files(&app), 2);
}

#[test]
fn receipt_lost_is_ambiguous_and_never_resent() {
    let (db, app, _home, now) = setup();
    let sent = Arc::new(AtomicUsize::new(0));
    let task = make_task(&db, "meeting-invite", "gmail", &now);
    let id = email_approval(&db, "p0-e", &now);
    link(&db, &task, &id, &now);
    let service = ApprovalService::with_sender(
        &app,
        Arc::new(UnknownSender {
            sent: Arc::clone(&sent),
        }),
    );
    let outcome = service.approve_and_execute(&id, "", false, &now).unwrap();
    assert!(
        matches!(outcome, ServiceOutcome::Ambiguous(_)),
        "lost receipt must be ambiguous, got {outcome:?}"
    );
    assert_eq!(sent.load(Ordering::SeqCst), 1, "the send happened once");
    assert_eq!(
        ApprovalQueue::new(&db).get(&id).unwrap().status,
        ApprovalStatus::NeedsReview,
        "the approval waits for human reconciliation"
    );
    assert_eq!(ledger_status(&db, "p0-e"), "in_progress");
    // No later path resends: not a second approve, not the recovery
    // path, not the worker settle.
    assert!(matches!(
        service.approve_and_execute(&id, "", false, &now).unwrap(),
        ServiceOutcome::Refused(_)
    ));
    assert!(matches!(
        service.execute_decided(&id, false, &now).unwrap(),
        ServiceOutcome::Refused(_)
    ));
    assert!(service.settle_task_approved(&task, &now).unwrap().is_empty());
    assert_eq!(sent.load(Ordering::SeqCst), 1, "still exactly one send");
}

#[test]
fn definite_failure_vs_unknown_outcome() {
    let (db, app, _home, now) = setup();
    // Definite refusal: the supplier said no. The approval is a
    // failure, the ledger records it as failed, and a fresh approval
    // for the same payload may retry (nothing was sent).
    let sent = Arc::new(AtomicUsize::new(0));
    let task = make_task(&db, "meeting-invite", "gmail", &now);
    let refused_id = email_approval(&db, "p0-f1", &now);
    link(&db, &task, &refused_id, &now);
    let rejecting = ApprovalService::with_sender(
        &app,
        Arc::new(RejectingSender {
            sent: Arc::clone(&sent),
        }),
    );
    let outcome = rejecting
        .approve_and_execute(&refused_id, "", false, &now)
        .unwrap();
    assert!(
        matches!(outcome, ServiceOutcome::Failed(_)),
        "definite refusal must be a failure, got {outcome:?}"
    );
    assert_eq!(
        ApprovalQueue::new(&db).get(&refused_id).unwrap().status,
        ApprovalStatus::Failed
    );
    assert_eq!(ledger_status(&db, "p0-f1"), "failed");
    let retry_id = email_approval(&db, "p0-f2", &now);
    link(&db, &task, &retry_id, &now);
    let outcome = ApprovalService::with_sender(
        &app,
        Arc::new(CountingSender {
            sent: Arc::clone(&sent),
        }),
    )
    .approve_and_execute(&retry_id, "", false, &now)
    .unwrap();
    assert!(matches!(outcome, ServiceOutcome::Executed(_)), "{outcome:?}");
    assert_eq!(sent.load(Ordering::SeqCst), 2, "one refused attempt + one retry");
    // Unknown outcome: same setup, a different branch — the approval
    // is not a failure and the ledger stays open.
    let unknown_id = email_approval(&db, "p0-f3", &now);
    link(&db, &task, &unknown_id, &now);
    let outcome = ApprovalService::with_sender(
        &app,
        Arc::new(UnknownSender {
            sent: Arc::clone(&sent),
        }),
    )
    .approve_and_execute(&unknown_id, "", false, &now)
    .unwrap();
    assert!(
        matches!(outcome, ServiceOutcome::Ambiguous(_)),
        "unknown outcome must be ambiguous, got {outcome:?}"
    );
    assert_eq!(
        ApprovalQueue::new(&db).get(&unknown_id).unwrap().status,
        ApprovalStatus::NeedsReview
    );
    assert_eq!(ledger_status(&db, "p0-f3"), "in_progress");
    assert_eq!(sent.load(Ordering::SeqCst), 3);
}

#[test]
fn worker_resume_settles_decided_approval() {
    let (db, app, _home, _setup_now) = setup();
    let dir = world_dir();
    let world = World::load(&dir).unwrap();
    let now = parse_rfc3339(&world.persona.now).unwrap();
    let out = TempDir::new().unwrap();
    let opts = |task_id: Option<String>| WorkerOpts {
        task_id,
        kind: None,
        lease_ttl_secs: 1,
        now_override: Some(now),
        kill: KillPoint::None,
        router: None,
        app: Some(Arc::clone(&app)),
        max_model_calls: None,
    };
    // First run: triage + gather run, then the artifact step blocks
    // on its write approval.
    let first = run_worker(&db, &dir, out.path(), opts(None)).unwrap();
    assert!(first.blocked.is_some(), "第一轮应在审批上暂停");
    let task_id = first.task_id.clone();
    let approval_id = UnitStore::new(&db)
        .get_step(&task_id, "artifact")
        .unwrap()
        .unwrap()
        .approval_id;
    assert!(!out.path().join("brief-v1.md").exists(), "批准前不得写文件");
    // Crash after decide: the decision is persisted, the execution
    // never ran.
    let status = ApprovalQueue::new(&db)
        .decide(&approval_id, true, "drill: crash after decide", &now)
        .unwrap();
    assert_eq!(status, ApprovalStatus::Approved);
    assert!(!out.path().join("brief-v1.md").exists());
    // Second run: the resumed worker settles the decided approval
    // through the shared service before dispatching, and the task
    // completes.
    let second = run_worker(&db, &dir, out.path(), opts(Some(task_id.clone()))).unwrap();
    assert_eq!(
        TaskStore::new(&db).get(&task_id).unwrap().unwrap().status,
        TaskStatus::Done,
        "resumed worker must finish the task (blocked: {:?})",
        second.blocked
    );
    assert!(out.path().join("brief-v1.md").exists(), "the settled approval wrote the brief");
    assert_eq!(
        ApprovalQueue::new(&db).get(&approval_id).unwrap().status,
        ApprovalStatus::Executed
    );
    let settled = EventLog::new(&db)
        .events_since(&task_id, 0, 1000)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "approval_settled")
        .count();
    assert_eq!(settled, 1, "one visible settle event for the recovery");
}
