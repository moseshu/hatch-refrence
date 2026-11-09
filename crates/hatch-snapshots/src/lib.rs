//! File snapshots: copy file contents into `$HATCH_HOME/snapshots/<id>/`
//! with a manifest, restore them by id.
//!
//! Layout mirrors the artifact store (one directory per object, manifest
//! next to the content): `<root>/<id>/manifest.json` plus
//! `<root>/<id>/files/<n>` — one stored copy per listed path, numbered by
//! position, so a hostile or sloppy filename can never escape the snapshot
//! directory. The manifest records the original absolute path, the sha256
//! and size of the stored bytes, and whether the path existed at all: a
//! path that did not exist yet is recorded as absent, and restoring it
//! removes the file if one was created since (that is what "back to the
//! snapshot" means for a file the step created).
//!
//! Snapshots are explicit, user-initiated data. Restoring is an explicit
//! action, never automatic — and it first snapshots the *current* state
//! of the same paths (`pre-restore of <id>`), so a restore can itself be
//! undone by restoring the pre-restore snapshot.
//!
//! Trust: manifests live under `HATCH_HOME` next to the database, the
//! same trust level as the plan and session tables. A manifest copied in
//! from elsewhere is not validated beyond its shape — do not load
//! snapshot directories from untrusted sources.
//!
//! Retention: deliberately none in this module. Snapshots are small in
//! number (one per high-risk plan step) and explicit; a future knob can
//! delete the oldest by `created_at` — [`SnapshotStore::list`] already
//! returns them newest-first, so the pruning itself is a few lines.
//! Nothing here deletes a snapshot on its own.

pub mod store;

pub use store::{RestoreReport, SnapshotFile, SnapshotManifest, SnapshotStore};

/// Snapshot ids are `snp_<16 hex>` (see [`hatch_core::ids::random_id`]).
/// Anything else is refused before it can touch the filesystem, so a
/// crafted id can never walk out of the snapshot root.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}
