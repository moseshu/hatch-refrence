//! End to end: a checkpoint (session fork + file snapshot) restores the
//! conversation and the files together; the fork keeps the full history.

use hatch_agent::checkpoint::{
    create_checkpoint, get_checkpoint, list_checkpoints, restore_checkpoint,
};
use hatch_agent::SessionStore;
use hatch_core::time::now;
use hatch_core::Trust;
use hatch_llm::ChatMessage;
use hatch_snapshots::SnapshotStore;
use hatch_store::Db;
use std::fs;

fn session_with(db: &Db, messages: &[&str]) -> String {
    let store = SessionStore::new(db);
    let now = now();
    let id = store.create("t", &now).unwrap();
    for m in messages {
        store.append(&id, &ChatMessage::user(*m), Trust::User, &now).unwrap();
    }
    id
}

#[test]
fn checkpoint_restores_session_and_files_together() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_in_memory().unwrap();
    let snapshots = SnapshotStore::new(&dir.path().join("snapshots"));
    let file = dir.path().join("notes.txt");
    fs::write(&file, "v1").unwrap();

    let session = session_with(&db, &["one", "two"]);
    let now = now();
    let checkpoint =
        create_checkpoint(&db, &snapshots, &session, std::slice::from_ref(&file), "before risky step", &now)
            .unwrap();
    assert_eq!(checkpoint.seq, 2);
    assert_eq!(checkpoint.session_id, session);

    // Drift: two more turns + an edited file.
    let store = SessionStore::new(&db);
    store.append(&session, &ChatMessage::user("three"), Trust::User, &now).unwrap();
    store.append(&session, &ChatMessage::user("four"), Trust::User, &now).unwrap();
    fs::write(&file, "v2").unwrap();
    assert_eq!(store.transcript(&session).unwrap().len(), 4);

    let report = restore_checkpoint(&db, &snapshots, &checkpoint.id, &now).unwrap();
    assert_eq!(report.rewind.removed, 2);
    assert_eq!(report.restore.restored, vec![file.display().to_string()]);
    assert_eq!(store.transcript(&session).unwrap().len(), 2);
    assert_eq!(fs::read_to_string(&file).unwrap(), "v1");
    // The fork still holds the full pre-checkpoint conversation.
    assert_eq!(store.transcript(&checkpoint.fork_id).unwrap().len(), 2);
    // The pre-restore fork holds the drifted conversation (4 turns).
    assert_eq!(
        store.transcript(&report.pre_restore_fork_id).unwrap().len(),
        4
    );
    // And the checkpoint is listed.
    assert_eq!(list_checkpoints(&snapshots).unwrap().len(), 1);
    assert_eq!(get_checkpoint(&snapshots, &checkpoint.id).unwrap().id, checkpoint.id);
}

#[test]
fn failed_file_restore_is_audited_with_fork_state() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_in_memory().unwrap();
    let snapshots = SnapshotStore::new(&dir.path().join("snapshots"));
    let file = dir.path().join("notes.txt");
    fs::write(&file, "v1").unwrap();

    let session = session_with(&db, &["one", "two"]);
    let now = now();
    let checkpoint =
        create_checkpoint(&db, &snapshots, &session, std::slice::from_ref(&file), "before risky step", &now)
            .unwrap();
    let store = SessionStore::new(&db);
    store.append(&session, &ChatMessage::user("three"), Trust::User, &now).unwrap();
    fs::write(&file, "v2").unwrap();

    // Corrupt the stored copy so the file half aborts on its hash
    // check — after the session half has already rewound.
    let stored_dir = dir.path().join("snapshots").join(&checkpoint.snapshot_id).join("files");
    let stored = fs::read_dir(&stored_dir).unwrap().next().unwrap().unwrap().path();
    fs::write(&stored, "tampered").unwrap();

    let err = restore_checkpoint(&db, &snapshots, &checkpoint.id, &now).unwrap_err();
    assert!(matches!(err, hatch_core::Error::Invalid(_)), "{err:?}");
    // Half-restored state is visible, not silent: session rewound,
    // file untouched, and the audit log names the failed step.
    assert_eq!(store.transcript(&session).unwrap().len(), 2);
    assert_eq!(fs::read_to_string(&file).unwrap(), "v2");
    let audit = db.audit_tail(10).unwrap();
    let entry = audit.iter().find(|a| a.event == "restore_failed").expect("audit entry");
    assert!(entry.detail.contains("file restore step failed"), "{}", entry.detail);
    assert!(entry.detail.contains("intact"), "{}", entry.detail);
}

#[test]
fn unknown_checkpoint_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_in_memory().unwrap();
    let snapshots = SnapshotStore::new(&dir.path().join("snapshots"));
    assert!(matches!(
        restore_checkpoint(&db, &snapshots, "chk_nope", &now()),
        Err(hatch_core::Error::NotFound(_))
    ));
    assert!(matches!(
        get_checkpoint(&snapshots, "chk_nope"),
        Err(hatch_core::Error::NotFound(_))
    ));
    assert!(list_checkpoints(&snapshots).unwrap().is_empty());
}
