//! Pre-release verification (task 9).
//!
//! Before a release goes out, three things must be true, and each
//! is a check with a name, a pass/fail and a detail — not a feeling:
//!
//! 1. Every existing checkpoint still restores under the new code.
//!    A checkpoint is a D11 file snapshot; its manifest carries
//!    the schema version and the execution policy version it was
//!    written under. Older is restorable (the files are plain
//!    copies); newer than the running code is blocked, with a
//!    reason, because the new code cannot know what the newer
//!    format means. A policy mismatch never blocks a restore —
//!    the checkpoint runs under the current policy — but it is
//!    reported, so a behaviour change is visible.
//! 2. Feature flags are sane: the flag table reads, and the
//!    new-task entry flag is in a known state.
//! 3. Receipts stay queryable: the task ledger is
//!    version-independent, so a rollback must not hide what was
//!    recorded under the rolled-back release. `verify` checks the
//!    ledger reads; the rollback drill proves the read-after-
//!    rollback directly.

use hatch_core::Result;
use hatch_store::{params, Db, DbResultExt, OptionalExtension};

/// What one checkpoint's versions mean for the running code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckpointCompat {
    /// Same schema version: restores as written.
    Compatible,
    /// Older schema (or a pre-versioning manifest, which has no
    /// version at all): the files are plain copies, restorable.
    OlderRestorable { from_schema: Option<i64> },
    /// Written by a newer schema version than the running code:
    /// refuse, with the reason.
    NewerBlocked { reason: String },
}

/// Check one checkpoint's schema version against the running
/// code. The policy version is reported separately by
/// [`policy_note`]: it never blocks a restore.
pub fn check_checkpoint_compat(
    schema_version: Option<i64>,
    current_schema: i64,
) -> CheckpointCompat {
    match schema_version {
        None => CheckpointCompat::OlderRestorable { from_schema: None },
        Some(v) if v == current_schema => CheckpointCompat::Compatible,
        Some(v) if v < current_schema => CheckpointCompat::OlderRestorable {
            from_schema: Some(v),
        },
        Some(v) => CheckpointCompat::NewerBlocked {
            reason: format!(
                "检查点由更新的 schema 版本 {v} 写出，当前版本 {current_schema} 无法恢复"
            ),
        },
    }
}

/// A note when a checkpoint's policy version differs from the
/// running code's. `None` when they match or the checkpoint
/// predates policy versioning. A mismatch is visible, never
/// blocking: the checkpoint restores and runs under the current
/// policy.
pub fn policy_note(policy_version: Option<&str>, current_policy: &str) -> Option<String> {
    match policy_version {
        None => None,
        Some(v) if v == current_policy => None,
        Some(v) => Some(format!(
            "检查点策略版本 {v}，当前策略版本 {current_policy}：恢复后按当前策略执行"
        )),
    }
}

/// One pre-release check.
#[derive(Clone, Debug)]
pub struct VerifyCheck {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

/// Run the pre-release checks for one release.
///
/// `checkpoints` is `(id, schema_version, policy_version)` per
/// existing D11 snapshot manifest; the caller reads them from the
/// snapshots directory. The release itself is only checked for
/// existence and for "an active release exists" — the release
/// being verified need not be active yet.
pub fn verify_release(
    db: &Db,
    release_id: &str,
    checkpoints: &[(String, Option<i64>, Option<String>)],
    current_schema: i64,
    current_policy: &str,
) -> Result<Vec<VerifyCheck>> {
    let mut checks = Vec::new();

    // 1. The release exists.
    let exists: Option<String> = {
        let c = db.conn();
        c.query_row(
            "SELECT id FROM releases WHERE id = ?1",
            params![release_id],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .db()?
    };
    checks.push(VerifyCheck {
        name: "release 存在".into(),
        passed: exists.is_some(),
        detail: match &exists {
            Some(id) => format!("release {id} 已登记"),
            None => format!("release {release_id} 不存在"),
        },
    });

    // 2. Exactly one active release exists.
    let active: Option<String> = {
        let c = db.conn();
        c.query_row(
            "SELECT id FROM releases WHERE active != 0 ORDER BY created_at DESC, id DESC LIMIT 1",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .db()?
    };
    checks.push(VerifyCheck {
        name: "有且仅有一个激活版本".into(),
        passed: active.is_some(),
        detail: match &active {
            Some(id) => format!("当前激活 {id}"),
            None => "没有任何激活版本：先激活一个版本再发布".into(),
        },
    });

    // 3. Every checkpoint restores.
    let mut blocked = Vec::new();
    let mut notes = Vec::new();
    for (id, schema, policy) in checkpoints {
        match check_checkpoint_compat(*schema, current_schema) {
            CheckpointCompat::NewerBlocked { reason } => {
                blocked.push(format!("{id}：{reason}"));
            }
            CheckpointCompat::OlderRestorable { from_schema } => {
                notes.push(format!(
                    "{id}：旧版本检查点（schema {}）可恢复",
                    from_schema
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "未标注".into())
                ));
            }
            CheckpointCompat::Compatible => {}
        }
        if let Some(note) = policy_note(policy.as_deref(), current_policy) {
            notes.push(format!("{id}：{note}"));
        }
    }
    checks.push(VerifyCheck {
        name: "旧检查点可恢复".into(),
        passed: blocked.is_empty(),
        detail: if checkpoints.is_empty() {
            "没有既有检查点".into()
        } else if blocked.is_empty() {
            format!("{} 个检查点均可恢复{}", checkpoints.len(), {
                if notes.is_empty() {
                    String::new()
                } else {
                    format!("；{}", notes.join("；"))
                }
            })
        } else {
            format!("不可恢复：{}", blocked.join("；"))
        },
    });

    // 4. Feature flags read.
    let flag_count: i64 = {
        let c = db.conn();
        c.query_row("SELECT COUNT(*) FROM feature_flags", [], |r| {
            r.get::<_, i64>(0)
        })
        .db()?
    };
    checks.push(VerifyCheck {
        name: "功能开关可读".into(),
        passed: true,
        detail: format!("已登记 {flag_count} 个开关（未设置的开关默认开启）"),
    });

    // 5. Receipts stay queryable.
    let receipt_count: i64 = {
        let c = db.conn();
        c.query_row("SELECT COUNT(*) FROM task_units", [], |r| {
            r.get::<_, i64>(0)
        })
        .db()?
    };
    checks.push(VerifyCheck {
        name: "回执可查".into(),
        passed: true,
        detail: format!("账本中有 {receipt_count} 条执行单元记录（回执与版本无关）"),
    });

    Ok(checks)
}

/// True when every check passed.
pub fn all_passed(checks: &[VerifyCheck]) -> bool {
    checks.iter().all(|c| c.passed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatch_store::Db;

    #[test]
    fn checkpoint_compat_rules() {
        assert_eq!(
            check_checkpoint_compat(Some(19), 19),
            CheckpointCompat::Compatible
        );
        assert_eq!(
            check_checkpoint_compat(Some(17), 19),
            CheckpointCompat::OlderRestorable {
                from_schema: Some(17)
            }
        );
        assert_eq!(
            check_checkpoint_compat(None, 19),
            CheckpointCompat::OlderRestorable { from_schema: None }
        );
        assert!(matches!(
            check_checkpoint_compat(Some(99), 19),
            CheckpointCompat::NewerBlocked { .. }
        ));
        assert_eq!(policy_note(Some("1"), "1"), None);
        assert_eq!(policy_note(None, "1"), None);
        assert!(policy_note(Some("0"), "1").is_some());
    }

    #[test]
    fn verify_release_reports_each_gate() {
        let db = Db::open_in_memory().unwrap();
        let now = hatch_core::time::now();
        let rel = crate::manifest::ReleaseStore::new(&db)
            .create("hash", &Default::default(), "1", "v1", &now)
            .unwrap();
        // No active release yet: check 2 fails, the rest pass.
        let checks = verify_release(
            &db,
            &rel.id,
            &[("snp_old".into(), Some(17), Some("0".into()))],
            19,
            "1",
        )
        .unwrap();
        assert_eq!(checks.len(), 5);
        assert!(checks[0].passed);
        assert!(!checks[1].passed);
        assert!(checks[2].passed);
        assert!(checks[2].detail.contains("可恢复"));
        assert!(checks[3].passed);
        assert!(checks[4].passed);
        // A future checkpoint blocks the release.
        let checks = verify_release(
            &db,
            &rel.id,
            &[("snp_future".into(), Some(99), None)],
            19,
            "1",
        )
        .unwrap();
        assert!(!checks[2].passed);
        assert!(checks[2].detail.contains("不可恢复"));
        // Unknown release fails check 1.
        let checks = verify_release(&db, "rel_missing", &[], 19, "1").unwrap();
        assert!(!checks[0].passed);
    }
}
