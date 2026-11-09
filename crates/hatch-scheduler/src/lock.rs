//! Single-instance lock: a pid file created with `O_EXCL`.
//!
//! A pid file left behind by a process that no longer exists is broken and
//! replaced; a live owner makes [`PidLock::acquire`] fail. The file is removed
//! when the lock is dropped (only if it still holds our pid).

use hatch_core::{Error, Result};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct PidLock {
    path: PathBuf,
    pid: u32,
}

/// Whether a process with this pid exists (`kill(pid, 0)`).
pub fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 performs only the existence/permission check.
    let r = unsafe { libc::kill(pid, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Freeze a process in place (`SIGSTOP`): it stops executing at once
/// and keeps all of its state (memory, open files, sockets). A stopped
/// process is still alive — [`pid_alive`] stays true — and resumes with
/// [`continue_process`]. This is the D5 pause mechanism for sub-agent
/// children: freezing the agent loop loses nothing, unlike killing it.
/// Only the process itself stops, not any grandchildren it spawned
/// (a child is not put in its own process group — the same granularity
/// as today's kill on cancel/timeout).
pub fn stop_process(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: SIGSTOP to a pid we own (a child we spawned).
    unsafe { libc::kill(pid, libc::SIGSTOP) == 0 }
}

/// Resume a process frozen by [`stop_process`] (`SIGCONT`).
pub fn continue_process(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: SIGCONT to a pid we own (a child we spawned).
    unsafe { libc::kill(pid, libc::SIGCONT) == 0 }
}

/// Kill a process (`SIGKILL`): it dies at once, even if it was
/// stopped by [`stop_process`]. Best-effort: false when the pid is
/// gone or cannot be signalled.
pub fn kill_process(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: SIGKILL to a pid we own (a child we spawned, or a
    // stopped orphan of a dead owner being reaped by reconciliation).
    unsafe { libc::kill(pid, libc::SIGKILL) == 0 }
}

/// The parent pid of a process, from `/proc/<pid>/stat` field 4 (after
/// the comm in parentheses, which may itself contain spaces). `None`
/// when the process is gone or `/proc` is unavailable.
pub fn parent_pid(pid: i32) -> Option<i32> {
    if pid <= 0 {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rfind(')')?;
    let fields: Vec<&str> = stat[after_comm + 2..].split_whitespace().collect();
    // fields[0] is state (field 3); fields[1] is ppid (field 4).
    fields.get(1)?.parse().ok()
}

/// Whether a process's owner is still alive: its parent pid exists,
/// is not pid 1 (reparented to init = the owner died), and answers to
/// signal 0. Used by sub-agent reconciliation for paused runs: a
/// stopped child cannot watch its socket, so a dead owner leaves a
/// frozen orphan that only this check can spot.
pub fn owner_alive(pid: i32) -> bool {
    match parent_pid(pid) {
        Some(ppid) if ppid > 1 => pid_alive(ppid),
        _ => false,
    }
}

pub fn read_pid(path: &Path) -> Option<i32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

impl PidLock {
    pub fn acquire(path: &Path) -> Result<PidLock> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let pid = std::process::id();
        for _ in 0..2 {
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(mut f) => {
                    writeln!(f, "{pid}")?;
                    f.sync_all()?;
                    return Ok(PidLock { path: path.to_path_buf(), pid });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => match read_pid(path) {
                    Some(other) if other as u32 != pid && pid_alive(other) => {
                        return Err(Error::Invalid(format!("daemon 已在运行（pid {other}，锁文件 {}）", path.display())));
                    }
                    // Dead owner, our own stale file, or garbage: break the lock.
                    _ => std::fs::remove_file(path)?,
                },
                Err(e) => return Err(e.into()),
            }
        }
        Err(Error::Invalid(format!("无法获取锁 {}", path.display())))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PidLock {
    fn drop(&mut self) {
        if read_pid(&self.path) == Some(self.pid as i32) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_fails_dead_owner_is_broken_and_drop_releases() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("daemon.pid");
        let lock = PidLock::acquire(&p).unwrap();
        assert_eq!(read_pid(&p), Some(std::process::id() as i32));
        // A live foreign owner (pid 1 always exists) blocks acquisition.
        drop(lock);
        assert!(!p.exists());
        std::fs::write(&p, "1\n").unwrap();
        assert!(PidLock::acquire(&p).is_err());
        // A dead owner is broken.
        std::fs::write(&p, "999999999\n").unwrap();
        let lock = PidLock::acquire(&p).unwrap();
        assert_eq!(read_pid(lock.path()), Some(std::process::id() as i32));
    }

    /// A live process can be frozen and resumed, and its owner is this
    /// test process while it lives. The child is killed on every path
    /// (a Drop guard: without it, a panic after the stop would leak a
    /// stopped child holding the test runner's pipes open).
    #[test]
    fn stop_continue_and_owner_alive_on_a_real_process() {
        struct Guard(std::process::Child);
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut child = Guard(
            std::process::Command::new("sleep")
                .arg("60")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("sleep must exist for this test"),
        );
        let pid = child.0.id() as i32;
        // A state transition after a signal is not instant: the kernel
        // stops/continues the task asynchronously, so poll briefly
        // instead of asserting the state of a single read.
        let state_is = |want: &str| -> bool {
            let state = std::fs::read_to_string(format!("/proc/{pid}/stat"));
            let Ok(state) = state else { return false };
            let Some(after_comm) = state.rfind(')') else { return false };
            state[after_comm + 2..].split_whitespace().next() == Some(want)
        };
        let wait_state = |want: &str| {
            for _ in 0..100 {
                if state_is(want) {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            false
        };
        let wait_not_state = |want: &str| {
            for _ in 0..100 {
                if !state_is(want) {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            false
        };
        assert!(pid_alive(pid));
        assert_eq!(parent_pid(pid), Some(std::process::id() as i32));
        assert!(owner_alive(pid));
        assert!(stop_process(pid));
        assert!(wait_state("T"), "a stopped process reports state T");
        assert!(pid_alive(pid), "a stopped process is still alive");
        assert!(owner_alive(pid), "its owner is still this process");
        assert!(continue_process(pid));
        assert!(wait_not_state("T"), "a continued process leaves state T");
        let _ = child.0.kill();
        let _ = child.0.wait();
        assert!(!pid_alive(pid));
        assert!(!stop_process(pid));
        assert!(!continue_process(pid));
        assert_eq!(parent_pid(pid), None);
        assert!(!owner_alive(pid));
        assert!(!pid_alive(0) && !pid_alive(-1));
        assert!(!stop_process(0) && !continue_process(-1));
    }

    #[test]
    fn orphaned_process_has_no_live_owner() {
        // The background job is a shell that redirects its own fds to
        // /dev/null and then stops *itself*: the stop is deterministic
        // (no `kill -STOP $!` race — stopping a freshly forked child
        // can land before it execs, leaving a stopped `sh` that still
        // holds the `output()` pipes open, so `output()` waits for an
        // EOF that never comes). The outer shell exits after 0.2s, so
        // the stopped job is reparented to pid 1: its owner is gone.
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg("sh -c 'exec >/dev/null 2>&1; kill -STOP $$; sleep 60' & pid=$!; echo $pid; sleep 0.2")
            .output()
            .expect("sh must exist for this test");
        let pid: i32 = String::from_utf8_lossy(&out.stdout).trim().parse().expect("the helper prints the sleeper pid");
        assert!(pid_alive(pid), "the orphaned sleeper is still alive");
        assert_eq!(parent_pid(pid), Some(1), "an orphan is reparented to pid 1");
        assert!(!owner_alive(pid));
        // Clean up the sleeper we orphaned (it is stopped, so kill works).
        unsafe {
            assert_eq!(libc::kill(pid, libc::SIGKILL), 0);
        }
    }
}
