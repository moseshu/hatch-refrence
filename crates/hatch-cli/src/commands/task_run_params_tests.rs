//! Unit tests for `task_run_params`: create-body parsing and the
//! idempotency repair.

#[cfg(test)]
mod tests {
    use crate::commands::task_run_params::*;
    use serde_json::json;
    use std::path::{Path, PathBuf};

    fn world_dir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let world = dir.path().join("world");
        std::fs::create_dir(&world).unwrap();
        (dir, world)
    }

    fn body(world: &Path) -> serde_json::Value {
        json!({
            "kind": "meeting-brief",
            "source": "fixture",
            "world": world.to_string_lossy(),
            "title": "API 简报",
        })
    }

    #[test]
    fn canonical_json_hashes_the_same_regardless_of_field_order() {
        let (_dir, world) = world_dir();
        let a = RunParams::from_create_body(&body(&world)).unwrap();
        let mut reordered = json!({
            "title": "API 简报",
            "world": world.to_string_lossy(),
            "source": "fixture",
            "kind": "meeting-brief",
        });
        // A different title is different content.
        let b = RunParams::from_create_body(&reordered).unwrap();
        assert_eq!(a.payload_hash().unwrap(), b.payload_hash().unwrap());
        reordered["title"] = json!("另一标题");
        let c = RunParams::from_create_body(&reordered).unwrap();
        assert_ne!(a.payload_hash().unwrap(), c.payload_hash().unwrap());
    }

    #[test]
    fn validation_mirrors_cli_run_flags() {
        let (_dir, world) = world_dir();
        // Unknown kind, unknown source, gmail without a user, a
        // gmail_max outside 1..=20, and a fixture run without a
        // world are all refused — the same refusals as `task run`.
        let mut bad = body(&world);
        bad["kind"] = json!("nope");
        assert!(RunParams::from_create_body(&bad).is_err());
        let mut bad = body(&world);
        bad["source"] = json!("nope");
        assert!(RunParams::from_create_body(&bad).is_err());
        let mut bad = body(&world);
        bad["source"] = json!("gmail");
        assert!(RunParams::from_create_body(&bad).is_err());
        let mut bad = body(&world);
        bad["source"] = json!("gmail");
        bad["gmail_user"] = json!("user@example.com");
        bad["gmail_max"] = json!(21);
        assert!(RunParams::from_create_body(&bad).is_err());
        let mut bad = body(&world);
        bad.as_object_mut().unwrap().remove("world");
        assert!(RunParams::from_create_body(&bad).is_err());
        // A gmail invite needs its three fields.
        let mut bad = body(&world);
        bad["kind"] = json!("meeting-invite");
        bad["source"] = json!("gmail");
        bad["gmail_user"] = json!("user@example.com");
        bad.as_object_mut().unwrap().remove("world");
        assert!(RunParams::from_create_body(&bad).is_err());
        bad["invite"] = json!({
            "event_title": "会",
            "event_start": "2026-11-16 10:00",
            "attendee_email": "a@example.com",
        });
        assert!(RunParams::from_create_body(&bad).is_ok());
    }

    #[test]
    fn gmail_run_fills_cli_defaults() {
        let (_dir, world) = world_dir();
        let mut b = body(&world);
        b["kind"] = json!("meeting-brief");
        b["source"] = json!("gmail");
        b["gmail_user"] = json!("user@example.com");
        b.as_object_mut().unwrap().remove("world");
        // No explicit title: the gmail default applies.
        b.as_object_mut().unwrap().remove("title");
        let p = RunParams::from_create_body(&b).unwrap();
        let run = p.to_gmail_run().unwrap();
        assert_eq!(run.user_email, "user@example.com");
        assert_eq!(run.user_name, "user@example.com");
        assert_eq!(run.title, "简报：最近邮件");
        assert!(run.prompt.contains("简报"));
    }
}
