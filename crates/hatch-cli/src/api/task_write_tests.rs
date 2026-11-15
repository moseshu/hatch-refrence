//! Tests for the task write endpoints ([`super::task_write`]):
//! create + idempotency, validation, the new-task-entry flag, and
//! the pause / resume / command roundtrip. Each test drives a real
//! local daemon over HTTP (see [`super::server_tests`]).

use super::server_tests::{authed, env, raw, start};
use hatch_core::time::now;
use hatch_tasks::{TaskStatus, TaskStore};
use serde_json::Value;
use std::sync::atomic::Ordering;

/// `POST /v1/tasks` — 202 + the task id, and the execution
/// parameters are persisted (schema v20) for the resident worker.
#[test]
fn api_create_returns_202_and_persists_params() {
    let env = env();
    let (addr, shutdown) = start(&env, Some("tok-123".into()), 20);
    let auth = authed("tok-123");
    let auth_refs: Vec<(&str, &str)> = auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let world = tempfile::TempDir::new().unwrap();
    let body = serde_json::json!({
        "kind": "meeting-brief",
        "title": "会前准备",
        "world": world.path().to_string_lossy(),
        "idempotency_key": "api-test-1",
    })
    .to_string();
    let (status, _, resp) = raw(addr, "POST", "/v1/tasks", &auth_refs, &body);
    assert_eq!(status, 202);
    let v: Value = serde_json::from_str(&resp).unwrap();
    let id = v["id"].as_str().unwrap().to_string();
    assert_eq!(v["duplicate"], false);
    assert_eq!(v["kind"], "meeting-brief");
    let task = TaskStore::new(&env.ctx.app.db).get(&id).unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Queued);
    assert_eq!(task.source, "fixture");
    let params = crate::commands::task_run_params::RunParams::load(&env.ctx.app.db, &id)
        .unwrap()
        .expect("执行参数应已持久化");
    assert_eq!(params.kind, "meeting-brief");
    assert_eq!(params.world, Some(world.path().to_path_buf()));
    assert_eq!(params.source, "fixture");
    shutdown.store(true, Ordering::SeqCst);
}

/// Idempotency: same key + same content → the same task
/// (`duplicate: true`); same key + different content → 409;
/// no key → two identical submits are two tasks.
#[test]
fn api_create_idempotency() {
    let env = env();
    let (addr, shutdown) = start(&env, Some("tok-123".into()), 20);
    let auth = authed("tok-123");
    let auth_refs: Vec<(&str, &str)> = auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let world = tempfile::TempDir::new().unwrap();
    let world_s = world.path().to_string_lossy().to_string();
    let body = serde_json::json!({
        "kind": "meeting-brief",
        "title": "会前准备",
        "world": world_s,
        "idempotency_key": "api-test-1",
    })
    .to_string();
    let (status, _, first) = raw(addr, "POST", "/v1/tasks", &auth_refs, &body);
    assert_eq!(status, 202);
    let first: Value = serde_json::from_str(&first).unwrap();
    let first_id = first["id"].as_str().unwrap().to_string();
    let (status, _, second) = raw(addr, "POST", "/v1/tasks", &auth_refs, &body);
    assert_eq!(status, 202);
    let second: Value = serde_json::from_str(&second).unwrap();
    assert_eq!(second["id"].as_str().unwrap(), first_id);
    assert_eq!(second["duplicate"], true);
    assert_eq!(
        TaskStore::new(&env.ctx.app.db)
            .list(None, 100)
            .unwrap()
            .len(),
        1
    );
    let changed = serde_json::json!({
        "kind": "meeting-brief",
        "title": "改了标题",
        "world": world_s,
        "idempotency_key": "api-test-1",
    })
    .to_string();
    let (status, _, body) = raw(addr, "POST", "/v1/tasks", &auth_refs, &changed);
    assert_eq!(status, 409);
    assert!(body.contains(&first_id));
    assert_eq!(
        TaskStore::new(&env.ctx.app.db)
            .list(None, 100)
            .unwrap()
            .len(),
        1
    );
    let no_key = serde_json::json!({
        "kind": "meeting-brief",
        "title": "无幂等键",
        "world": world_s,
    })
    .to_string();
    let (status, _, v1) = raw(addr, "POST", "/v1/tasks", &auth_refs, &no_key);
    assert_eq!(status, 202);
    let (status, _, v2) = raw(addr, "POST", "/v1/tasks", &auth_refs, &no_key);
    assert_eq!(status, 202);
    let v1: Value = serde_json::from_str(&v1).unwrap();
    let v2: Value = serde_json::from_str(&v2).unwrap();
    assert_ne!(v1["id"].as_str().unwrap(), v2["id"].as_str().unwrap());
    shutdown.store(true, Ordering::SeqCst);
}

/// Validation: unknown kind, missing world, and a world dir
/// that does not exist are all 400s.
#[test]
fn api_create_validation_rejects_bad_input() {
    let env = env();
    let (addr, shutdown) = start(&env, Some("tok-123".into()), 20);
    let auth = authed("tok-123");
    let auth_refs: Vec<(&str, &str)> = auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let (status, _, _) = raw(
        addr,
        "POST",
        "/v1/tasks",
        &auth_refs,
        r#"{"kind": "not-a-kind", "world": "/tmp"}"#,
    );
    assert_eq!(status, 400);
    let (status, _, _) = raw(
        addr,
        "POST",
        "/v1/tasks",
        &auth_refs,
        r#"{"kind": "meeting-brief"}"#,
    );
    assert_eq!(status, 400);
    let (status, _, _) = raw(
        addr,
        "POST",
        "/v1/tasks",
        &auth_refs,
        r#"{"kind": "meeting-brief", "world": "/does/not/exist"}"#,
    );
    assert_eq!(status, 400);
    shutdown.store(true, Ordering::SeqCst);
}

/// With the task-9 new-task entry closed, API create is refused
/// but reads stay open; reopening lets the same body succeed.
#[test]
fn api_create_flag_off_refuses_but_reads_stay_open() {
    let env = env();
    let (addr, shutdown) = start(&env, Some("tok-123".into()), 20);
    let auth = authed("tok-123");
    let auth_refs: Vec<(&str, &str)> = auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
    hatch_release::FlagStore::new(&env.ctx.app.db)
        .set(hatch_release::NEW_TASK_ENTRY, false, &now())
        .unwrap();
    let world = tempfile::TempDir::new().unwrap();
    let body = serde_json::json!({
        "kind": "meeting-brief",
        "title": "会前准备",
        "world": world.path().to_string_lossy(),
    })
    .to_string();
    let (status, _, resp) = raw(addr, "POST", "/v1/tasks", &auth_refs, &body);
    assert_eq!(status, 400);
    assert!(resp.contains("新任务入口已关闭"));
    assert!(TaskStore::new(&env.ctx.app.db)
        .list(None, 100)
        .unwrap()
        .is_empty());
    let (status, _, _) = raw(addr, "GET", "/v1/tasks", &auth_refs, "");
    assert_eq!(status, 200);
    hatch_release::FlagStore::new(&env.ctx.app.db)
        .set(hatch_release::NEW_TASK_ENTRY, true, &now())
        .unwrap();
    let (status, _, _) = raw(addr, "POST", "/v1/tasks", &auth_refs, &body);
    assert_eq!(status, 202);
    shutdown.store(true, Ordering::SeqCst);
}

/// Pause / resume / command over HTTP, on the same ledger the
/// CLI reads: pause → blocked, resume → queued, command with
/// the current version bumps it, a stale one is refused.
#[test]
fn api_pause_resume_command_roundtrip() {
    let env = env();
    let (addr, shutdown) = start(&env, Some("tok-123".into()), 20);
    let auth = authed("tok-123");
    let auth_refs: Vec<(&str, &str)> = auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let world = tempfile::TempDir::new().unwrap();
    let body = serde_json::json!({
        "kind": "meeting-brief",
        "title": "会前准备",
        "world": world.path().to_string_lossy(),
    })
    .to_string();
    let (status, _, resp) = raw(addr, "POST", "/v1/tasks", &auth_refs, &body);
    assert_eq!(status, 202);
    let id = serde_json::from_str::<Value>(&resp).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, _, _) = raw(
        addr,
        "POST",
        &format!("/v1/tasks/{id}/pause"),
        &auth_refs,
        "{}",
    );
    assert_eq!(status, 200);
    assert_eq!(
        TaskStore::new(&env.ctx.app.db)
            .get(&id)
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Blocked
    );
    let (status, _, _) = raw(
        addr,
        "POST",
        &format!("/v1/tasks/{id}/resume"),
        &auth_refs,
        "{}",
    );
    assert_eq!(status, 200);
    assert_eq!(
        TaskStore::new(&env.ctx.app.db)
            .get(&id)
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Queued
    );
    let (status, _, resp) = raw(
        addr,
        "POST",
        &format!("/v1/tasks/{id}/command"),
        &auth_refs,
        r#"{"expected_version": 1, "summary": "把标题改成周报"}"#,
    );
    assert_eq!(status, 200);
    assert!(resp.contains("v2"));
    let (status, _, _) = raw(
        addr,
        "POST",
        &format!("/v1/tasks/{id}/command"),
        &auth_refs,
        r#"{"expected_version": 1, "summary": "过期的指令"}"#,
    );
    assert_eq!(status, 400);
    let (status, _, resp) = raw(addr, "GET", &format!("/v1/tasks/{id}"), &auth_refs, "");
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["plan_version"], 2);
    shutdown.store(true, Ordering::SeqCst);
}
