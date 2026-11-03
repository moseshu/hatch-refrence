//! Integration tests for the connector adapter: tool naming/classes,
//! direct-call refusal, and the `connector_call` executor end-to-end
//! (plain spawn and vault-helper injection).

use hatch_connectors::framework::{manifest::OpDef, ConnectorManifest};
use hatch_policy::ActionClass;
use hatch_tools::executors::ExecContext;
use hatch_tools::{
    ActionExecutor, ConnectorCallExecutor, ConnectorTool, Tool, ToolContext,
};
use hatch_vault::client::StoreRequest;
use serde_json::json;
use std::path::PathBuf;

/// Tests in this module share one process env; serialize the ones
/// that set env vars so a guard drop in one cannot unset a var
/// another is using mid-call.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn manifest() -> ConnectorManifest {
    ConnectorManifest {
        name: "notes".into(),
        command: "true".into(),
        ops: vec![
            OpDef { name: "list_notes".into(), description: "List.".into(), read_only: true, args_schema: json!({}) },
            OpDef { name: "write_note".into(), description: "Write.".into(), read_only: false, args_schema: json!({}) },
        ],
        ..Default::default()
    }
}

#[test]
fn tool_names_and_classes() {
    let m = manifest();
    let read = ConnectorTool::new(&m, "list_notes").unwrap();
    assert_eq!(read.name(), "connector_notes_list_notes");
    assert_eq!(read.class(), ActionClass::FetchWeb);
    let write = ConnectorTool::new(&m, "write_note").unwrap();
    assert_eq!(write.class(), ActionClass::ExecCommand);
    assert!(ConnectorTool::new(&m, "nope").is_none());
}

#[test]
fn direct_call_is_refused() {
    let m = manifest();
    let tool = ConnectorTool::new(&m, "list_notes").unwrap();
    let ctx = ToolContext {
        db: &hatch_store::Db::open_in_memory().unwrap(),
        now: hatch_core::time::now(),
        tz: chrono_tz::UTC,
        user_email: String::new(),
        origin: "test".into(),
        tainted: false,
        call_id: "1".into(),
        cancel: hatch_core::CancelToken::new(),
    };
    assert!(tool.call(&json!({}), &ctx).is_err());
    let approval = tool.to_approval(&json!({}), &ctx).unwrap().unwrap();
    assert_eq!(approval.kind, "connector_call");
}

#[test]
fn executor_runs_notes_connector_end_to_end() {
    let _lock = ENV_LOCK.lock().unwrap();
    // Install the notes manifest + binary path into a temp home.
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("connectors");
    std::fs::create_dir_all(&dir).unwrap();
    let notes_bin = notes_bin_path();
    let notes_dir = home.path().join("notes");
    std::fs::create_dir_all(&notes_dir).unwrap();
    std::fs::write(
        dir.join("notes.toml"),
        format!(
            "name = \"notes\"\ncommand = \"{notes_bin}\"\n[[ops]]\nname = \"write_note\"\nread_only = false\n[[ops]]\nname = \"read_note\"\nread_only = true\n"
        ),
    )
    .unwrap();
    // The executor resolves env from the process env in this test.
    let _env = EnvGuard::set(
        "HATCH_NOTES_DIR",
        notes_dir.to_string_lossy().as_ref(),
    );
    let exec = ConnectorCallExecutor::with_dir(home.path(), &dir);
    let ctx = ExecContext {
        db: &hatch_store::Db::open_in_memory().unwrap(),
        now: hatch_core::time::now(),
        approval_id: "a",
        origin: "test",
        cancel: None,
    };
    let out = exec
        .execute(
            &json!({"connector": "notes", "op": "write_note", "arguments": {"name": "hi", "content": "yo"}}),
            &ctx,
        )
        .unwrap();
    assert!(out.contains("written"), "{out}");
    let out = exec
        .execute(
            &json!({"connector": "notes", "op": "read_note", "arguments": {"name": "hi"}}),
            &ctx,
        )
        .unwrap();
    assert!(out.contains("yo"), "{out}");
}

#[test]
fn executor_injects_token_via_vault_helper() {
    let _lock = ENV_LOCK.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("connectors");
    std::fs::create_dir_all(&dir).unwrap();
    let notes_bin = notes_bin_path();
    let notes_dir = home.path().join("notes");
    std::fs::create_dir_all(&notes_dir).unwrap();
    // The notes connector ignores the token, but the executor must
    // still resolve it through the helper (inject path) for the call
    // to succeed at all.
    std::fs::write(
        dir.join("notes.toml"),
        format!(
            "name = \"notes\"\ncommand = \"{notes_bin}\"\ntoken_ref = \"notes-token\"\n[[ops]]\nname = \"list_notes\"\nread_only = true\n"
        ),
    )
    .unwrap();
    let _env = EnvGuard::set("HATCH_NOTES_DIR", notes_dir.to_string_lossy().as_ref());
    let vc = hatch_vault::VaultClient::start(home.path()).unwrap();
    vc.store(
        "notes-token",
        StoreRequest { value: "test-token-notes".into(), ..Default::default() },
    )
    .unwrap();
    drop(vc);
    let exec = ConnectorCallExecutor::with_dir(home.path(), &dir);
    let ctx = ExecContext {
        db: &hatch_store::Db::open_in_memory().unwrap(),
        now: hatch_core::time::now(),
        approval_id: "a",
        origin: "test",
        cancel: None,
    };
    let out = exec
        .execute(
            &json!({"connector": "notes", "op": "list_notes", "arguments": {}}),
            &ctx,
        )
        .unwrap();
    assert!(out.contains("notes"), "{out}");
}

#[test]
fn executor_fails_cleanly_when_vault_token_missing() {
    let _lock = ENV_LOCK.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("connectors");
    std::fs::create_dir_all(&dir).unwrap();
    let notes_bin = notes_bin_path();
    std::fs::write(
        dir.join("notes.toml"),
        format!(
            "name = \"notes\"\ncommand = \"{notes_bin}\"\ntoken_ref = \"missing-token\"\n[[ops]]\nname = \"list_notes\"\nread_only = true\n"
        ),
    )
    .unwrap();
    let exec = ConnectorCallExecutor::with_dir(home.path(), &dir);
    let ctx = ExecContext {
        db: &hatch_store::Db::open_in_memory().unwrap(),
        now: hatch_core::time::now(),
        approval_id: "a",
        origin: "test",
        cancel: None,
    };
    let e = exec
        .execute(
            &json!({"connector": "notes", "op": "list_notes", "arguments": {}}),
            &ctx,
        )
        .unwrap_err();
    assert!(e.to_string().contains("not_found"), "{e}");
}

/// Locate (building on demand) the notes reference connector binary.
fn notes_bin_path() -> String {
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_hatch-notes-connector") {
        return p;
    }
    let plain = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/hatch-notes-connector");
    if plain.is_file() {
        return plain.to_string_lossy().into_owned();
    }
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let ok = std::process::Command::new(&cargo)
        .args(["build", "-p", "hatch-connectors", "--bin", "hatch-notes-connector"])
        .current_dir(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap(),
        )
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok && plain.is_file(), "could not build hatch-notes-connector");
    plain.to_string_lossy().into_owned()
}

/// Restores an env var on drop (tests share one process env).
struct EnvGuard {
    key: String,
    old: Option<String>,
}

impl EnvGuard {
    fn set(key: &str, value: &str) -> Self {
        let old = std::env::var(key).ok();
        std::env::set_var(key, value);
        EnvGuard { key: key.to_string(), old }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.old {
            Some(v) => std::env::set_var(&self.key, v),
            None => std::env::remove_var(&self.key),
        }
    }
}
