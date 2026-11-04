//! In-process API server tests, continued: approvals, jobs,
//! artifacts, skills/connectors, goals/feed/ideas/calendar/mail.
//! Harness lives in `server_tests`.

use super::server_tests::{authed, env, raw, start};
use std::sync::atomic::Ordering;

    #[test]
    fn approvals_list_show_approve_deny() {
        use hatch_policy::{ActionClass, ApprovalQueue, NewApproval};
        let env = env();
        let (addr, shutdown) = start(&env, Some("tok-123".into()), 20);
        let auth = authed("tok-123");
        let auth: Vec<(&str, &str)> =
            auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let (status, _, body) = raw(addr, "GET", "/v1/approvals", &auth, "");
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["approvals"],
            serde_json::json!([])
        );
        let (status, _, _) = raw(addr, "GET", "/v1/approvals/apr_nope", &auth, "");
        assert_eq!(status, 404);
        let (status, _, _) =
            raw(addr, "POST", "/v1/approvals/apr_nope/approve", &auth, "{}");
        assert_eq!(status, 404);
        let (status, _, _) = raw(addr, "POST", "/v1/approvals/apr_nope/deny", &auth, "{}");
        assert_eq!(status, 404);
        // A real write_file approval, queued directly: approving it over
        // HTTP must execute into the temp workspace, and a second
        // approve must be a 409, never a second execution.
        let q = ApprovalQueue::new(&env.ctx.app.db);
        let submit = |key: &str, path: &str| {
            q.submit(
                &NewApproval {
                    idempotency_key: key.into(),
                    kind: "write_file".into(),
                    class: ActionClass::WriteFile,
                    title: format!("write {path}"),
                    preview: format!("write {path}"),
                    payload: serde_json::json!({
                        "root": env._dir.path(),
                        "path": path,
                        "content": "hello",
                        "base_hash": null,
                    }),
                    reasons: vec!["test".into()],
                    origin: "api:test".into(),
                    ttl: chrono::Duration::hours(24),
                },
                &hatch_core::time::now(),
            )
            .unwrap()
            .id()
            .to_string()
        };
        let id = submit("api-test-write", "api-test.txt");
        let (status, _, body) = raw(addr, "GET", &format!("/v1/approvals/{id}"), &auth, "");
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["payload_intact"],
            true
        );
        let (status, _, body) =
            raw(addr, "POST", &format!("/v1/approvals/{id}/approve"), &auth, "{}");
        assert_eq!(status, 200, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["outcome"], "executed");
        assert!(env._dir.path().join("api-test.txt").exists());
        let (status, _, _) =
            raw(addr, "POST", &format!("/v1/approvals/{id}/approve"), &auth, "{}");
        assert_eq!(status, 409, "already decided: never execute twice");
        let id2 = submit("api-test-write-2", "api-test-2.txt");
        let (status, _, body) =
            raw(addr, "POST", &format!("/v1/approvals/{id2}/deny"), &auth, r#"{"note":"no"}"#);
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["status"],
            "rejected"
        );
        assert!(!env._dir.path().join("api-test-2.txt").exists());
        let (status, _, _) =
            raw(addr, "POST", &format!("/v1/approvals/{id2}/approve"), &auth, "{}");
        assert_eq!(status, 409, "denied approvals stay denied");
        shutdown.store(true, Ordering::SeqCst);
    }

    #[test]
    fn jobs_crud_roundtrip() {
        let env = env();
        let (addr, shutdown) = start(&env, Some("tok-123".into()), 20);
        let auth = authed("tok-123");
        let auth: Vec<(&str, &str)> =
            auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let (status, _, body) = raw(addr, "GET", "/v1/jobs", &auth, "");
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["jobs"],
            serde_json::json!([])
        );
        let (status, _, _) = raw(
            addr,
            "POST",
            "/v1/jobs",
            &auth,
            r#"{"name":"j1","kind":"bogus","schedule":"0 9 * * *"}"#,
        );
        assert_eq!(status, 400, "unknown kind is rejected");
        let (status, _, _) =
            raw(addr, "POST", "/v1/jobs", &auth, r#"{"name":"j1"}"#);
        assert_eq!(status, 400, "missing fields are rejected");
        let (status, _, body) = raw(
            addr,
            "POST",
            "/v1/jobs",
            &auth,
            r#"{"name":"j1","kind":"memory_flush","schedule":"0 9 * * *"}"#,
        );
        assert_eq!(status, 201, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let id = v["id"].as_str().unwrap().to_string();
        assert_eq!(v["name"], "j1");
        let (status, _, _) = raw(
            addr,
            "POST",
            "/v1/jobs",
            &auth,
            r#"{"name":"j1","kind":"memory_flush","schedule":"0 9 * * *"}"#,
        );
        assert_eq!(status, 400, "duplicate name is rejected");
        let (status, _, _) = raw(addr, "GET", &format!("/v1/jobs/{id}"), &auth, "");
        assert_eq!(status, 200);
        let (status, _, body) = raw(addr, "GET", &format!("/v1/jobs/{id}/runs"), &auth, "");
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["runs"],
            serde_json::json!([])
        );
        // Run now: memory_flush is safe on an empty db; whatever its
        // outcome, the run must be recorded in history.
        let (status, _, body) = raw(addr, "POST", &format!("/v1/jobs/{id}/run"), &auth, "");
        assert_eq!(status, 200, "{body}");
        let (status, _, body) = raw(addr, "GET", &format!("/v1/jobs/{id}/runs"), &auth, "");
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body)
                .unwrap()["runs"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let (status, _, _) = raw(
            addr,
            "PATCH",
            &format!("/v1/jobs/{id}"),
            &auth,
            r#"{"schedule":"garbage"}"#,
        );
        assert_eq!(status, 400, "bad schedule is rejected");
        let (status, _, body) = raw(
            addr,
            "PATCH",
            &format!("/v1/jobs/{id}"),
            &auth,
            r#"{"enabled":false,"schedule":"0 10 * * *"}"#,
        );
        assert_eq!(status, 200, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["enabled"], false);
        // Schedule serializes with its kind prefix (ser_display).
        assert_eq!(v["schedule"], "cron 0 10 * * *");
        let (status, _, _) = raw(addr, "DELETE", &format!("/v1/jobs/{id}"), &auth, "");
        assert_eq!(status, 200);
        let (status, _, _) = raw(addr, "GET", &format!("/v1/jobs/{id}"), &auth, "");
        assert_eq!(status, 404);
        shutdown.store(true, Ordering::SeqCst);
    }

    #[test]
    fn artifacts_create_get_share_revoke() {
        let env = env();
        let (addr, shutdown) = start(&env, Some("tok-123".into()), 20);
        let auth = authed("tok-123");
        let auth: Vec<(&str, &str)> =
            auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let (status, _, _) = raw(addr, "POST", "/v1/artifacts", &auth, "{}");
        assert_eq!(status, 400, "create needs title and content");
        let (status, _, _) = raw(
            addr,
            "POST",
            "/v1/artifacts",
            &auth,
            r#"{"title":"t","content":"hi","kind":"bogus"}"#,
        );
        assert_eq!(status, 400, "unknown kind is rejected");
        // Secret scan: an API-key-shaped string is refused before writing.
        let (status, _, body) = raw(
            addr,
            "POST",
            "/v1/artifacts",
            &auth,
            r#"{"title":"t","content":"leaked sk-abcdefghijklmnop1234 here"}"#,
        );
        assert_eq!(status, 400, "{body}");
        let (status, _, body) = raw(
            addr,
            "POST",
            "/v1/artifacts",
            &auth,
            r#"{"title":"hello doc","content":"hello world"}"#,
        );
        assert_eq!(status, 201, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let id = v["id"].as_str().unwrap().to_string();
        assert_eq!(v["version"], 1);
        assert_eq!(v["shared"], false);
        let (status, _, body) = raw(addr, "GET", &format!("/v1/artifacts/{id}"), &auth, "");
        assert_eq!(status, 200, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["content"], "hello world");
        assert_eq!(v["versions"], serde_json::json!([1]));
        let (status, _, _) = raw(addr, "GET", "/v1/artifacts/art_nope", &auth, "");
        assert_eq!(status, 404);
        // Share submits an approval; sharing again reuses the same one.
        let (status, _, body) =
            raw(addr, "POST", &format!("/v1/artifacts/{id}/share"), &auth, "");
        assert_eq!(status, 202, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let approval_id = v["approval_id"].as_str().unwrap().to_string();
        let (status, _, _) =
            raw(addr, "GET", &format!("/v1/approvals/{approval_id}"), &auth, "");
        assert_eq!(status, 200, "share approval is in the queue");
        let (status, _, body) =
            raw(addr, "POST", &format!("/v1/artifacts/{id}/share"), &auth, "");
        assert_eq!(status, 202, "{body}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["approval_id"],
            approval_id,
            "same idempotency key, same approval"
        );
        let (status, _, _) =
            raw(addr, "POST", "/v1/artifacts/art_nope/share", &auth, "");
        assert_eq!(status, 404);
        // Revocation is 501 until share-token persistence lands.
        let (status, _, body) =
            raw(addr, "POST", &format!("/v1/artifacts/{id}/revoke"), &auth, "");
        assert_eq!(status, 501, "{body}");
        assert!(body.contains("not_implemented"), "{body}");
        shutdown.store(true, Ordering::SeqCst);
    }

    #[test]
    fn skills_connectors_lists() {
        let env = env();
        let (addr, shutdown) = start(&env, Some("tok-123".into()), 20);
        let auth = authed("tok-123");
        let auth: Vec<(&str, &str)> =
            auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let (status, _, body) = raw(addr, "GET", "/v1/skills", &auth, "");
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["skills"],
            serde_json::json!([])
        );
        let (status, _, body) = raw(addr, "GET", "/v1/connectors", &auth, "");
        assert_eq!(status, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["connectors"], serde_json::json!([]));
        assert_eq!(v["warnings"], serde_json::json!([]));
        let (status, _, _) = raw(addr, "GET", "/v1/connectors/nope", &auth, "");
        assert_eq!(status, 404);
        // Drop a manifest (plus a broken one) into the temp home and
        // check the list picks them up, and the live status check runs.
        let dir = env._dir.path().join("connectors");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("good.toml"),
            "name = \"good\"\ncommand = \"true\"\n[[ops]]\nname = \"status\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("bad.toml"), "name = \nbroken toml [[[\n").unwrap();
        let (status, _, body) = raw(addr, "GET", "/v1/connectors", &auth, "");
        assert_eq!(status, 200, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let items = v["connectors"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["name"], "good");
        assert_eq!(items[0]["ops"][0]["name"], "status");
        assert_eq!(v["warnings"].as_array().unwrap().len(), 1);
        let (status, _, body) = raw(addr, "GET", "/v1/connectors/good", &auth, "");
        assert_eq!(status, 200, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        // `true` answers nothing, so the live check reports not-ok —
        // the point is the endpoint runs the check and reports it.
        assert_eq!(v["status"]["ok"], false);
        assert!(!v["status"]["detail"].as_str().unwrap().is_empty());
        shutdown.store(true, Ordering::SeqCst);
    }

    #[test]
    fn goals_feed_ideas_calendar_mail() {
        use hatch_connectors::{CalendarRepo, Event};
        use hatch_goals::{FeedStore, IdeaStore};
        let env = env();
        let (addr, shutdown) = start(&env, Some("tok-123".into()), 20);
        let auth = authed("tok-123");
        let auth: Vec<(&str, &str)> =
            auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let (status, _, body) = raw(addr, "GET", "/v1/goals", &auth, "");
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["goals"],
            serde_json::json!([])
        );
        let (status, _, _) = raw(addr, "POST", "/v1/goals", &auth, "{}");
        assert_eq!(status, 400, "create needs a title");
        let (status, _, body) =
            raw(addr, "POST", "/v1/goals", &auth, r#"{"title":"learn rust"}"#);
        assert_eq!(status, 201, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let gid = v["id"].as_str().unwrap().to_string();
        assert_eq!(v["status"], "active");
        let (status, _, body) = raw(addr, "GET", &format!("/v1/goals/{gid}"), &auth, "");
        assert_eq!(status, 200, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["title"], "learn rust");
        assert_eq!(v["files"], serde_json::json!([]));
        let (status, _, _) = raw(addr, "GET", "/v1/goals/goal_nope", &auth, "");
        assert_eq!(status, 404);
        // Seed one feed item, one idea and one event straight into the
        // stores, then check the read endpoints surface them.
        let now = hatch_core::time::now();
        FeedStore::new(&env.ctx.app.db)
            .save("feed title", "feed content", &["F1".into()], "test", false, &now)
            .unwrap();
        IdeaStore::new(&env.ctx.app.db)
            .add("test idea", "user", 1.0, &now)
            .unwrap();
        CalendarRepo::new(&env.ctx.app.db)
            .upsert(&Event {
                uid: "evt-1".into(),
                title: "standup".into(),
                starts_at: now + chrono::Duration::hours(1),
                ends_at: now + chrono::Duration::hours(2),
                location: String::new(),
                source: "test".into(),
            })
            .unwrap();
        let (status, _, body) = raw(addr, "GET", "/v1/feed", &auth, "");
        assert_eq!(status, 200);
        let items = serde_json::from_str::<serde_json::Value>(&body).unwrap()["feed"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["title"], "feed title");
        let (status, _, body) = raw(addr, "GET", "/v1/ideas", &auth, "");
        assert_eq!(status, 200);
        let items = serde_json::from_str::<serde_json::Value>(&body).unwrap()["ideas"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["text"], "test idea");
        let (status, _, body) = raw(addr, "GET", "/v1/calendar/events", &auth, "");
        assert_eq!(status, 200);
        let items = serde_json::from_str::<serde_json::Value>(&body).unwrap()["events"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["title"], "standup");
        let (status, _, body) =
            raw(addr, "GET", "/v1/calendar/events?days=abc", &auth, "");
        assert_eq!(status, 200, "bad days falls back to the default");
        let _ = body;
        let (status, _, body) = raw(addr, "GET", "/v1/mail/triage", &auth, "");
        assert_eq!(status, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["receipt"], serde_json::Value::Null);
        assert_eq!(v["drafts"], serde_json::json!([]));
        shutdown.store(true, Ordering::SeqCst);
    }
