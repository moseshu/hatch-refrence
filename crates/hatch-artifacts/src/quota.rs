//! Artifact quotas: how many artifacts, how many bytes in total,
//! how big one version may be.
//!
//! Single-user v1: the quota counts the whole home, no per-user
//! split (that is multi-tenancy work). A limit of `0` means
//! unlimited for that dimension. The check runs inside
//! [`crate::store::ArtifactStore::build`] *before* anything is
//! written, so a refused build never leaves a half-written version
//! behind.
//!
//! Defaults (also the `[artifacts]` config defaults): 200 artifacts,
//! 100 MiB total, 5 MiB per file. Artifacts are markdown/HTML
//! documents; the caps are generous for that shape and only stop
//! runaway growth (a model looping `create_artifact`, a pasted dump).

use hatch_core::{Error, Result};
use std::path::Path;

/// Quota limits. `0` = unlimited for that dimension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quota {
    pub max_artifacts: usize,
    pub max_total_bytes: u64,
    pub max_file_bytes: u64,
}

impl Default for Quota {
    fn default() -> Quota {
        Quota {
            max_artifacts: 200,
            max_total_bytes: 100 * 1024 * 1024,
            max_file_bytes: 5 * 1024 * 1024,
        }
    }
}

/// Current usage of one artifact store.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub count: usize,
    pub total_bytes: u64,
}

/// Human-readable byte count (`512 B`, `1.5 MiB`).
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// Scan the store: count artifacts (directories with a `meta.json`)
/// and sum the bytes of every version's content file. Best effort —
/// an unreadable entry is skipped, not fatal: usage is a guardrail,
/// and a corrupt entry is reported by the store's own reads.
pub fn usage(root: &Path) -> Result<Usage> {
    let mut out = Usage::default();
    let Ok(rd) = std::fs::read_dir(root) else {
        return Ok(out);
    };
    for entry in rd.flatten() {
        let dir = entry.path();
        if !dir.is_dir() || !dir.join("meta.json").is_file() {
            continue;
        }
        out.count += 1;
        let Ok(vd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for v in vd.flatten() {
            let vdir = v.path();
            if !vdir.is_dir() || !v.file_name().to_string_lossy().starts_with('v') {
                continue;
            }
            let Ok(files) = std::fs::read_dir(&vdir) else {
                continue;
            };
            for f in files.flatten() {
                let p = f.path();
                if p.is_file() && p.file_name().map(|n| n != "manifest.json").unwrap_or(false) {
                    out.total_bytes += f.metadata().map(|m| m.len()).unwrap_or(0);
                }
            }
        }
    }
    Ok(out)
}

/// Refuse a build that would break a limit. The error names which
/// limit, the current usage and the cap — never just "quota exceeded".
/// `is_new` is true for a first version (a new artifact counts
/// against the count limit; a new version of an existing artifact
/// does not, but its bytes still count).
pub fn check(quota: &Quota, usage: &Usage, is_new: bool, new_bytes: u64) -> Result<()> {
    if quota.max_file_bytes > 0 && new_bytes > quota.max_file_bytes {
        return Err(Error::QuotaExceeded(format!(
            "单个文件 {} 超过上限 {}",
            human_bytes(new_bytes),
            human_bytes(quota.max_file_bytes)
        )));
    }
    if is_new && quota.max_artifacts > 0 && usage.count >= quota.max_artifacts {
        return Err(Error::QuotaExceeded(format!(
            "artifact 总数 {}/{} 已满（新 artifact 会超限；删掉旧的或调高 [artifacts] max_artifacts）",
            usage.count, quota.max_artifacts
        )));
    }
    if quota.max_total_bytes > 0
        && usage.total_bytes.saturating_add(new_bytes) > quota.max_total_bytes
    {
        return Err(Error::QuotaExceeded(format!(
            "artifact 总字节 {} + 新文件 {} 超过上限 {}（当前 {} 个 artifact）",
            human_bytes(usage.total_bytes),
            human_bytes(new_bytes),
            human_bytes(quota.max_total_bytes),
            usage.count
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{ArtifactKind, ArtifactStore};

    #[test]
    fn defaults_are_generous_but_bounded() {
        let q = Quota::default();
        assert_eq!(q.max_artifacts, 200);
        assert_eq!(q.max_total_bytes, 100 * 1024 * 1024);
        assert_eq!(q.max_file_bytes, 5 * 1024 * 1024);
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MiB");
    }

    #[test]
    fn each_limit_refuses_and_names_usage() {
        let usage = Usage {
            count: 199,
            total_bytes: 90 * 1024 * 1024,
        };
        let q = Quota::default();
        // Per-file limit.
        let e = check(&q, &usage, true, 6 * 1024 * 1024).unwrap_err();
        assert!(matches!(e, Error::QuotaExceeded(_)), "{e}");
        assert!(
            e.to_string().contains("6.0 MiB") && e.to_string().contains("5.0 MiB"),
            "{e}"
        );
        // Count limit (new artifact only).
        let e = check(
            &q,
            &Usage {
                count: 200,
                total_bytes: 0,
            },
            true,
            10,
        )
        .unwrap_err();
        assert!(e.to_string().contains("200/200"), "{e}");
        assert!(
            check(
                &q,
                &Usage {
                    count: 200,
                    total_bytes: 0
                },
                false,
                10
            )
            .is_ok(),
            "a new version of an existing artifact does not count"
        );
        // Total-bytes limit (new file itself fits the per-file cap).
        let big = Usage {
            count: 10,
            total_bytes: 98 * 1024 * 1024,
        };
        let e = check(&q, &big, false, 4 * 1024 * 1024).unwrap_err();
        assert!(
            e.to_string().contains("98.0 MiB") && e.to_string().contains("100.0 MiB"),
            "{e}"
        );
        // Zero means unlimited.
        let open = Quota {
            max_artifacts: 0,
            max_total_bytes: 0,
            max_file_bytes: 0,
        };
        assert!(check(
            &open,
            &Usage {
                count: 9999,
                total_bytes: u64::MAX / 2
            },
            true,
            1
        )
        .is_ok());
    }

    #[test]
    fn usage_scan_counts_artifacts_and_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let s = ArtifactStore::new(&dir.path().join("artifacts"));
        let b1 = s
            .build(None, "一", ArtifactKind::Document, "hello", "test")
            .unwrap();
        s.build(
            Some(&b1.meta.id),
            "一",
            ArtifactKind::Document,
            "hello world, again",
            "test",
        )
        .unwrap();
        s.build(None, "二", ArtifactKind::Document, "xyz", "test")
            .unwrap();
        let u = usage(&dir.path().join("artifacts")).unwrap();
        assert_eq!(u.count, 2, "two artifacts, three versions");
        assert_eq!(
            u.total_bytes,
            5 + 18 + 3,
            "content bytes only, manifests excluded"
        );
    }

    #[test]
    fn build_enforces_quota_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let s = ArtifactStore::new(&dir.path().join("artifacts")).with_quota(Quota {
            max_artifacts: 1,
            max_total_bytes: 100,
            max_file_bytes: 10,
        });
        s.build(None, "一", ArtifactKind::Document, "12345", "test")
            .unwrap();
        // Second artifact: count limit, and nothing is written.
        let e = s
            .build(None, "二", ArtifactKind::Document, "x", "test")
            .unwrap_err();
        assert!(matches!(e, Error::QuotaExceeded(_)), "{e}");
        assert_eq!(s.list().unwrap().len(), 1, "refused build writes nothing");
        // A new version of the first artifact is still allowed (bytes fit).
        s.build(
            Some(&s.list().unwrap()[0].id),
            "一",
            ArtifactKind::Document,
            "1234",
            "test",
        )
        .unwrap();
        // Per-file limit.
        let e = s
            .build(None, "三", ArtifactKind::Document, "12345678901", "test")
            .unwrap_err();
        assert!(e.to_string().contains("上限"), "{e}");
    }

    #[test]
    fn empty_root_is_zero_usage() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(usage(dir.path()).unwrap(), Usage::default());
    }
}
