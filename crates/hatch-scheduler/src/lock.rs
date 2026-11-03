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
}
