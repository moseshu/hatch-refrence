//! `hatch-sandbox` — runs commands with the isolation a personal assistant
//! needs before it may touch a shell.
//!
//! * [`spec`]    — the request sent to the helper and the isolation report.
//! * [`runner`]  — caller side: spawn, timeout (kills the whole tree), bounded output.
//! * [`helper`]  — the helper process chain (namespaces, new root, caps, seccomp).
//! * [`seccomp`] — the BPF deny-list.
//!
//! There is no silent fallback: if the host refuses namespaces the command is
//! not run, unless the user explicitly chose `SandboxMode::None`.

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("hatch-sandbox supports x86_64 and aarch64 Linux only");

pub mod helper;
pub mod runner;
pub mod seccomp;
pub mod spec;

pub use helper::helper_main;
pub use runner::{clean_env, HelperCmd, RunOutput, Sandbox, SandboxMode};
pub use spec::{IsolationReport, Limits};

impl HelperCmd {
    /// Locate a helper for tests and tools: `$HATCH_SANDBOX_HELPER`, else a
    /// `hatch-sandbox-helper` binary next to (or one level above) the current
    /// executable — where cargo puts it for test runs.
    pub fn locate() -> Option<HelperCmd> {
        if let Some(p) = std::env::var_os("HATCH_SANDBOX_HELPER").filter(|p| !p.is_empty()) {
            return Some(HelperCmd { program: p.into(), prefix_args: vec![] });
        }
        let exe = std::env::current_exe().ok()?;
        let dir = exe.parent()?;
        [dir.join("hatch-sandbox-helper"), dir.parent()?.join("hatch-sandbox-helper")]
            .into_iter()
            .find(|p| p.is_file())
            .map(|program| HelperCmd { program, prefix_args: vec![] })
    }
}
