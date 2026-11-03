//! Tests for hatch-vault. Separate file so `lib.rs` stays under 400 lines.

use crate::{is_valid_name, open, vault_dir, vault_path, Vault};
use hatch_core::Error;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn open_temp() -> (tempfile::TempDir, Vault) {
    let home = tempfile::tempdir().expect("tempdir");
    let vault = open(home.path()).expect("open vault");
    (home, vault)
}

#[test]
fn set_get_round_trip() {
    let (_home, vault) = open_temp();
    vault.set("github", "ghp-secret").unwrap();
    assert_eq!(vault.get("github").unwrap(), Some("ghp-secret".into()));
    assert_eq!(vault.get("missing").unwrap(), None);
}

#[test]
fn list_returns_names_only() {
    let (_home, vault) = open_temp();
    vault.set("alpha", "value-a").unwrap();
    vault.set("beta", "value-b").unwrap();
    let names = vault.list().unwrap();
    assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);
    // Names only: no value may leak into the listing.
    assert!(!names.iter().any(|n| n.contains("value")));
}

#[test]
fn delete_removes() {
    let (_home, vault) = open_temp();
    vault.set("temp", "x").unwrap();
    assert!(vault.delete("temp").unwrap());
    assert_eq!(vault.get("temp").unwrap(), None);
    assert!(!vault.delete("temp").unwrap());
    assert!(vault.list().unwrap().is_empty());
}

#[test]
fn invalid_names_rejected() {
    let (_home, vault) = open_temp();
    for bad in [
        "",
        "has space",
        "has/slash",
        "has.dot",
        "emoji-\u{1F600}",
        &"a".repeat(65),
    ] {
        assert!(!is_valid_name(bad), "{bad:?} should be invalid");
        assert!(
            vault.set(bad, "v").is_err(),
            "{bad:?} should be rejected by set"
        );
        assert!(vault.get(bad).is_err(), "{bad:?} should be rejected by get");
        assert!(
            vault.delete(bad).is_err(),
            "{bad:?} should be rejected by delete"
        );
    }
    assert!(is_valid_name("a"));
    assert!(is_valid_name("A-Z_a-z-0-9"));
    assert!(is_valid_name(&"a".repeat(64)));
    assert!(vault.set("ok_name-1", "v").is_ok());
    // Empty values are rejected too.
    assert!(vault.set("ok_name-1", "").is_err());
}

#[test]
fn corrupt_file_errors_instead_of_wiping() {
    let (_home, vault) = open_temp();
    fs::write(&vault.path, "{not valid json").unwrap();
    let err = vault.get("anything").unwrap_err();
    assert!(
        matches!(err, Error::Invalid(_)),
        "expected Invalid, got {err}"
    );
    // The corrupt file is left untouched, not wiped.
    assert_eq!(fs::read_to_string(&vault.path).unwrap(), "{not valid json");

    fs::write(&vault.path, "[1, 2, 3]").unwrap();
    assert!(matches!(vault.list().unwrap_err(), Error::Invalid(_)));
    fs::write(&vault.path, r#"{"a": 42}"#).unwrap();
    assert!(matches!(vault.list().unwrap_err(), Error::Invalid(_)));
}

#[test]
fn permissions_are_0600_0700() {
    let (_home, vault) = open_temp();
    assert_eq!(
        fs::metadata(&vault.dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&vault.path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    vault.set("k", "v").unwrap();
    // Atomic rewrite must preserve the 0600 file mode.
    assert_eq!(
        fs::metadata(&vault.path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(vault.check_permissions().is_empty());
}

#[test]
fn open_tightens_existing_loose_permissions() {
    let home = tempfile::tempdir().unwrap();
    let dir = vault_dir(home.path());
    fs::create_dir_all(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    let path = vault_path(home.path());
    fs::write(&path, "{}").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

    let vault = open(home.path()).unwrap();
    assert_eq!(
        fs::metadata(&vault.dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&vault.path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn check_permissions_reports_problems() {
    let (_home, vault) = open_temp();
    fs::set_permissions(&vault.dir, fs::Permissions::from_mode(0o755)).unwrap();
    let warnings = vault.check_permissions();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("0700"));

    fs::set_permissions(&vault.path, fs::Permissions::from_mode(0o644)).unwrap();
    let warnings = vault.check_permissions();
    assert_eq!(warnings.len(), 2);
    assert!(warnings.iter().any(|w| w.contains("0600")));

    fs::remove_file(&vault.path).unwrap();
    let warnings = vault.check_permissions();
    assert!(warnings.iter().any(|w| w.contains("missing")));
}

#[test]
fn set_overwrites() {
    let (_home, vault) = open_temp();
    vault.set("key", "first").unwrap();
    vault.set("key", "second").unwrap();
    assert_eq!(vault.get("key").unwrap(), Some("second".into()));
    assert_eq!(vault.list().unwrap(), vec!["key".to_string()]);
}

#[test]
fn get_of_unknown_name_is_none() {
    let (_home, vault) = open_temp();
    assert_eq!(vault.get("nope").unwrap(), None);
}

#[test]
fn vault_paths() {
    let home = Path::new("/home/testuser");
    assert_eq!(vault_dir(home), PathBuf::from("/home/testuser/vault"));
    assert_eq!(
        vault_path(home),
        PathBuf::from("/home/testuser/vault/tokens.json")
    );
}
