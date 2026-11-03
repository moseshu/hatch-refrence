//! What the helper is asked to do, and what it reports back.
//!
//! The spec travels from the caller to the helper as one JSON argument; the
//! helper never reads configuration from the environment (its environment is
//! cleared by the caller anyway).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Uid / gid the sandboxed command sees.
pub const SANDBOX_ID: u32 = 65534;

/// Marker prefixes on the helper's stderr.
pub const SETUP_ERROR_MARKER: &str = "HATCH_SANDBOX_SETUP_ERROR ";
pub const REPORT_MARKER: &str = "HATCH_SANDBOX_REPORT ";
/// Exit code used by the helper when isolation could not be set up.
pub const SETUP_ERROR_EXIT: i32 = 125;

/// Resource limits applied with `setrlimit` before `execve`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Limits {
    /// CPU seconds (RLIMIT_CPU).
    pub cpu_secs: u64,
    /// Address space in MiB (RLIMIT_AS).
    pub memory_mb: u64,
    /// Largest file the command may write, MiB (RLIMIT_FSIZE).
    pub file_size_mb: u64,
    /// Processes for the (host-side) user (RLIMIT_NPROC).
    pub max_processes: u64,
    /// Open files (RLIMIT_NOFILE).
    pub max_open_files: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { cpu_secs: 60, memory_mb: 2048, file_size_mb: 64, max_processes: 512, max_open_files: 256 }
    }
}

/// One sandboxed command.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Spec {
    /// Program and arguments (`argv[0]` is resolved through `PATH` inside).
    pub argv: Vec<String>,
    /// Absolute host path of the workspace; mounted read-write at the same path.
    pub workspace: PathBuf,
    /// Absolute working directory, inside `workspace`.
    pub cwd: PathBuf,
    /// Environment of the command (everything else is cleared).
    pub env: Vec<(String, String)>,
    /// Keep the host network (default false: a fresh, empty network namespace).
    #[serde(default)]
    pub network: bool,
    pub limits: Limits,
    /// Print an [`IsolationReport`] line on stderr before `execve`.
    #[serde(default)]
    pub report: bool,
}

/// Which isolation steps actually took effect.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct IsolationReport {
    pub user_ns: bool,
    pub mount_ns: bool,
    pub net_ns: bool,
    pub pid_ns: bool,
    pub ipc_ns: bool,
    pub uts_ns: bool,
    pub pivot_root: bool,
    /// `/proc` of the new pid namespace is mounted (optional: some hosts refuse).
    pub proc_mounted: bool,
    pub readonly_root: bool,
    pub caps_dropped: bool,
    pub no_new_privs: bool,
    pub seccomp: bool,
    pub rlimits: bool,
    /// Uid the command runs as inside the namespace.
    pub uid: u32,
    /// Notes about optional steps that were skipped.
    #[serde(default)]
    pub notes: Vec<String>,
}

impl IsolationReport {
    /// Human-readable one-liner, e.g. for `hatch doctor`.
    pub fn summary(&self) -> String {
        let items = [
            ("user", self.user_ns),
            ("mount", self.mount_ns),
            ("net", self.net_ns),
            ("pid", self.pid_ns),
            ("ipc", self.ipc_ns),
            ("uts", self.uts_ns),
            ("pivot_root", self.pivot_root),
            ("/proc", self.proc_mounted),
            ("ro-root", self.readonly_root),
            ("caps", self.caps_dropped),
            ("nnp", self.no_new_privs),
            ("seccomp", self.seccomp),
            ("rlimits", self.rlimits),
        ];
        let on: Vec<&str> = items.iter().filter(|(_, v)| *v).map(|(k, _)| *k).collect();
        let off: Vec<&str> = items.iter().filter(|(_, v)| !*v).map(|(k, _)| *k).collect();
        let mut s = format!("uid={} 生效: {}", self.uid, on.join(" "));
        if !off.is_empty() {
            s.push_str(&format!("；未生效: {}", off.join(" ")));
        }
        s
    }
}
