//! Share-token behaviour: mint / resolve / revoke / delete.
//! The registry file never holds a plaintext token; unknown and
//! revoked tokens are indistinguishable from the outside.

use hatch_artifacts::share::{
    delete_artifact, resolve_share, revoke_artifact_shares, ShareRegistry, TOKEN_PREFIX,
};
use hatch_artifacts::store::{ArtifactKind, ArtifactStore};
use hatch_core::Error;

fn setup() -> (tempfile::TempDir, ArtifactStore, ShareRegistry) {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("artifacts");
    let store = ArtifactStore::new(&root);
    let reg = ShareRegistry::new(&root);
    (d, store, reg)
}

fn built_artifact(store: &ArtifactStore) -> String {
    let b = store
        .build(None, "周报", ArtifactKind::Document, "内容", "test")
        .unwrap();
    store.mark_shared(&b.meta.id, "appr_1").unwrap();
    b.meta.id
}

#[test]
fn mint_resolve_revoke_round_trip() {
    let (_d, store, reg) = setup();
    let id = built_artifact(&store);
    let (rec, token) = reg.mint(&id, Some("appr_1")).unwrap();
    assert!(token.starts_with(TOKEN_PREFIX));
    assert_eq!(token.len(), TOKEN_PREFIX.len() + 64);
    assert_eq!(reg.resolve(&token).unwrap().unwrap().id, rec.id);
    // The token resolves to the latest content.
    let (got, manifest, content) = resolve_share(&store, &token).unwrap();
    assert_eq!(got.id, rec.id);
    assert_eq!(manifest.version, 1);
    assert_eq!(content, "内容");
    // Revoke: the same token is now a 404, and the artifact flips unshared.
    let revoked = revoke_artifact_shares(&store, &id).unwrap();
    assert_eq!(revoked, 1);
    assert!(reg.resolve(&token).unwrap().is_none());
    assert!(matches!(
        resolve_share(&store, &token).unwrap_err(),
        Error::NotFound(_)
    ));
    assert!(
        !store.get(&id).unwrap().shared,
        "no active shares left: unshared"
    );
}

#[test]
fn unknown_token_is_indistinguishable_from_revoked() {
    let (_d, store, reg) = setup();
    let id = built_artifact(&store);
    let (_rec, token) = reg.mint(&id, None).unwrap();
    let mut bad = token.clone();
    bad.push('0');
    assert!(reg.resolve(&bad).unwrap().is_none());
    assert!(reg.resolve("hatch_art_0000").unwrap().is_none());
    assert!(matches!(
        resolve_share(&store, "hatch_art_0000").unwrap_err(),
        Error::NotFound(_)
    ));
}

#[test]
fn registry_file_never_holds_plaintext() {
    let (_d, _store, reg) = setup();
    let (_rec, token) = reg.mint("art_x", None).unwrap();
    let raw = std::fs::read_to_string(reg.path()).unwrap();
    assert!(
        !raw.contains(&token),
        "plaintext token must never be stored"
    );
    assert!(
        !raw.contains("hatch_art_"),
        "no token-shaped string in the registry"
    );
    assert!(raw.contains("token_hash"), "only the hash is stored");
}

#[test]
fn corrupt_registry_fails_closed() {
    let (_d, _store, reg) = setup();
    std::fs::create_dir_all(reg.path().parent().unwrap()).unwrap();
    std::fs::write(reg.path(), "{not json").unwrap();
    assert!(matches!(
        reg.mint("art_x", None).unwrap_err(),
        Error::Storage(_)
    ));
    assert!(matches!(
        reg.resolve("hatch_art_x").unwrap_err(),
        Error::Storage(_)
    ));
}

#[test]
fn mint_dedups_by_approval() {
    let (_d, _store, reg) = setup();
    let (first, token) = reg.mint_or_existing("art_x", Some("appr_9")).unwrap();
    assert!(token.is_some(), "first mint shows the token once");
    let (again, token) = reg.mint_or_existing("art_x", Some("appr_9")).unwrap();
    assert_eq!(again.id, first.id);
    assert!(token.is_none(), "re-run must not mint a second token");
    assert_eq!(reg.list().unwrap().len(), 1);
}

#[test]
fn delete_artifact_revokes_its_shares() {
    let (_d, store, reg) = setup();
    let id = built_artifact(&store);
    let (_r1, _t1) = reg.mint(&id, None).unwrap();
    let (_r2, t2) = reg.mint(&id, None).unwrap();
    let other = store
        .build(None, "二", ArtifactKind::Document, "x", "test")
        .unwrap()
        .meta
        .id;
    let (_r3, t3) = reg.mint(&other, None).unwrap();
    let revoked = delete_artifact(&store, &id).unwrap();
    assert_eq!(revoked, 2);
    assert!(store.get(&id).is_err(), "directory is gone");
    assert!(reg.resolve(&t2).unwrap().is_none());
    assert!(
        reg.resolve(&t3).unwrap().is_some(),
        "other artifact's share survives"
    );
    assert!(reg.list_for(&id).unwrap().iter().all(|r| r.revoked));
}
