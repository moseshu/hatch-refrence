//! Public-API tests for the snapshot store: a restore can itself be
//! undone by restoring the pre-restore snapshot it recorded.

use hatch_core::time::now;
use hatch_snapshots::SnapshotStore;
use std::fs;

#[test]
fn restore_can_be_undone_via_pre_restore_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("notes.txt");
    fs::write(&file, "original").unwrap();
    let store = SnapshotStore::new(&dir.path().join("snapshots"));
    let snap = store.create(std::slice::from_ref(&file), "test", &now()).unwrap();

    fs::write(&file, "edited").unwrap();
    let report = store.restore(&snap.id, &now()).unwrap();
    assert_eq!(fs::read_to_string(&file).unwrap(), "original");

    // Undo the restore: the pre-restore snapshot holds "edited".
    let undo = store.restore(&report.pre_restore_id, &now()).unwrap();
    assert_eq!(fs::read_to_string(&file).unwrap(), "edited");
    assert_eq!(undo.restored, vec![file.display().to_string()]);
}
