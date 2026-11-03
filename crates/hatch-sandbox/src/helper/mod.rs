//! The sandbox helper: a short-lived, single-threaded process chain.
//!
//! ```text
//! caller ──spawn──► P (helper, own process group, dies with caller)
//!                    │  writes C's uid/gid maps from the parent namespace
//!                    └─fork─► C  unshare(user|mount|ipc|uts|net|pid), dies with P
//!                              └─fork─► G  pid 1 of the new pid namespace, dies with C
//!                                         setsid · hostname · new root · rlimits
//!                                         drop caps · no_new_privs · seccomp · execve
//! ```
//!
//! Killing P's process group therefore takes the whole tree down: C dies, G
//! (pid 1) gets SIGKILL through its death signal, and the kernel tears down the
//! rest of the pid namespace. Any setup failure is reported on stderr with
//! [`SETUP_ERROR_MARKER`] and exit code [`SETUP_ERROR_EXIT`] — the command is
//! never run without the isolation that was asked for.

mod mounts;
mod privs;
mod sys;

use crate::spec::{IsolationReport, Spec, REPORT_MARKER, SANDBOX_ID, SETUP_ERROR_EXIT, SETUP_ERROR_MARKER};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use sys::SysResult;

/// Entry point for the helper binary / hidden subcommand. Never returns.
pub fn helper_main(args: &[String]) -> ! {
    let code = match parse(args).and_then(|spec| run_parent(&spec)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{SETUP_ERROR_MARKER}{e}");
            SETUP_ERROR_EXIT
        }
    };
    std::process::exit(code)
}

fn parse(args: &[String]) -> SysResult<Spec> {
    match args {
        [flag, json] if flag == "--spec" => serde_json::from_str(json).map_err(|e| format!("bad spec: {e}")),
        _ => Err("usage: --spec <json>".into()),
    }
}

fn staging_dir() -> SysResult<PathBuf> {
    // SAFETY: getpid has no preconditions.
    let pid = unsafe { libc::getpid() };
    let dir = std::env::temp_dir().join(format!("hatch-sbx-{pid}"));
    std::fs::create_dir_all(&dir).map_err(|e| format!("staging dir: {e}"))?;
    Ok(dir)
}

fn write_maps(child: libc::pid_t) -> SysResult<()> {
    // SAFETY: getuid/getgid have no preconditions.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let proc = PathBuf::from(format!("/proc/{child}"));
    std::fs::write(proc.join("setgroups"), "deny").map_err(|e| format!("setgroups: {e}"))?;
    std::fs::write(proc.join("uid_map"), format!("{SANDBOX_ID} {uid} 1")).map_err(|e| format!("uid_map: {e}"))?;
    std::fs::write(proc.join("gid_map"), format!("{SANDBOX_ID} {gid} 1")).map_err(|e| format!("gid_map: {e}"))?;
    Ok(())
}

/// P: create C, map its ids, relay its exit status.
fn run_parent(spec: &Spec) -> SysResult<i32> {
    // SAFETY: getppid has no preconditions.
    sys::die_with_parent(unsafe { libc::getppid() })?;
    if !spec.workspace.is_absolute() || !spec.cwd.starts_with(&spec.workspace) {
        return Err("workspace must be absolute and cwd inside it".into());
    }
    let staging = staging_dir()?;
    let (sync_r, sync_w) = sys::pipe_cloexec()?; // C → P: unshare result
    let (go_r, go_w) = sys::pipe_cloexec()?; // P → C: maps written
    let (err_r, err_w) = sys::pipe_cloexec()?; // C/G → P: setup errors (EOF on exec)
    // SAFETY: getpid has no preconditions.
    let me = unsafe { libc::getpid() };
    let pid = sys::fork()?;
    if pid == 0 {
        sys::close(sync_r);
        sys::close(go_w);
        sys::close(err_r);
        let code = match run_c(spec, &staging, me, sync_w, go_r, err_w) {
            Ok(code) => code,
            Err(e) => {
                sys::write_all(err_w, e.as_bytes());
                SETUP_ERROR_EXIT
            }
        };
        std::process::exit(code);
    }
    sys::close(sync_w);
    sys::close(go_r);
    sys::close(err_w);
    let result = (|| {
        match sys::read_byte(sync_r) {
            Some(b'U') => {}
            _ => return Err(None),
        }
        write_maps(pid).map_err(Some)?;
        sys::write_all(go_w, b"M");
        Ok(())
    })();
    sys::close(go_w);
    let setup_err = sys::read_to_end(err_r, 4096);
    let code = sys::wait_exit_code(pid);
    let _ = std::fs::remove_dir(&staging);
    if let Err(Some(e)) = result {
        return Err(e);
    }
    if !setup_err.is_empty() {
        return Err(String::from_utf8_lossy(&setup_err).into_owned());
    }
    if result.is_err() {
        return Err("sandbox child failed before namespace setup".into());
    }
    Ok(code)
}

/// C: enter the new namespaces, wait for the id maps, start G.
fn run_c(spec: &Spec, staging: &Path, parent: libc::pid_t, sync_w: i32, go_r: i32, err_w: i32) -> SysResult<i32> {
    sys::die_with_parent(parent)?;
    let mut flags = libc::CLONE_NEWUSER | libc::CLONE_NEWNS | libc::CLONE_NEWIPC | libc::CLONE_NEWUTS | libc::CLONE_NEWPID;
    if !spec.network {
        flags |= libc::CLONE_NEWNET;
    }
    // SAFETY: C is single-threaded.
    if unsafe { libc::unshare(flags) } != 0 {
        let e = sys::last("unshare(user|mount|ipc|uts|pid|net) — 此系统不允许非特权用户命名空间");
        sys::write_all(sync_w, b"E");
        return Err(e);
    }
    sys::write_all(sync_w, b"U");
    sys::close(sync_w);
    if sys::read_byte(go_r) != Some(b'M') {
        return Err("parent did not write the id maps".into());
    }
    sys::close(go_r);
    let g = sys::fork()?;
    if g == 0 {
        let e = run_g(spec, staging);
        sys::write_all(err_w, e.as_bytes());
        std::process::exit(SETUP_ERROR_EXIT);
    }
    sys::close(err_w);
    Ok(sys::wait_exit_code(g))
}

/// G: build the filesystem view, drop everything, exec. Returns only on error.
fn run_g(spec: &Spec, staging: &Path) -> String {
    let mut report = IsolationReport {
        user_ns: true,
        mount_ns: true,
        ipc_ns: true,
        uts_ns: true,
        pid_ns: true,
        net_ns: !spec.network,
        uid: SANDBOX_ID,
        ..Default::default()
    };
    let r = (|| -> SysResult<std::io::Error> {
        sys::die_with_parent_unchecked()?;
        // SAFETY: plain syscalls in a single-threaded process.
        unsafe {
            if libc::setsid() < 0 {
                return Err(sys::last("setsid"));
            }
            // Hostname isolation is best-effort: some hosts (e.g. GitHub
            // Actions runners) refuse sethostname inside the new UTS
            // namespace with EPERM. The UTS namespace itself is still
            // created, but without a renamed hostname we must not claim
            // full UTS isolation in the report. Everything else below
            // (mounts, caps, seccomp) still applies.
            let host = b"hatch-sandbox";
            if sys::check(libc::sethostname(host.as_ptr().cast(), host.len()), "sethostname").is_err() {
                report.uts_ns = false;
                eprintln!("warning: sethostname failed, continuing without hostname isolation");
            }
        }
        let last_cap = privs::last_cap();
        mounts::setup(&spec.workspace, staging, true, &mut report)?;
        std::env::set_current_dir(&spec.cwd).map_err(|e| format!("chdir {}: {e}", spec.cwd.display()))?;
        privs::apply_limits(&spec.limits)?;
        report.rlimits = true;
        privs::drop_all(last_cap)?;
        report.caps_dropped = true;
        report.no_new_privs = true;
        crate::seccomp::install()?;
        report.seccomp = true;
        if spec.report {
            eprintln!("{REPORT_MARKER}{}", serde_json::to_string(&report).unwrap_or_default());
        }
        let Some((prog, args)) = spec.argv.split_first() else {
            return Err("empty argv".into());
        };
        let err = std::process::Command::new(prog).args(args).env_clear().envs(spec.env.iter().map(|(k, v)| (k, v))).exec();
        Ok(err)
    })();
    match r {
        Ok(exec_err) => format!("execve {:?}: {exec_err}", spec.argv.first()),
        Err(e) => e,
    }
}
