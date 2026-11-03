//! Thin, checked wrappers around the raw syscalls the helper needs.

use std::ffi::CString;
use std::io::Error;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub type SysResult<T> = Result<T, String>;

pub fn last(what: &str) -> String {
    format!("{what}: {}", Error::last_os_error())
}

pub fn cstr(p: &Path) -> SysResult<CString> {
    CString::new(p.as_os_str().as_bytes()).map_err(|_| format!("path contains NUL: {}", p.display()))
}

pub fn check(rc: libc::c_int, what: &str) -> SysResult<()> {
    if rc == 0 {
        Ok(())
    } else {
        Err(last(what))
    }
}

/// Die with SIGKILL when the parent goes away; fail if it already has.
pub fn die_with_parent(expected_parent: libc::pid_t) -> SysResult<()> {
    // SAFETY: plain prctl/getppid calls.
    unsafe {
        check(libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong, 0, 0, 0), "prctl(PDEATHSIG)")?;
        if libc::getppid() != expected_parent {
            return Err("parent exited during sandbox setup".into());
        }
    }
    Ok(())
}

/// Death signal only: for pid 1 of a new pid namespace, whose parent lives
/// outside the namespace and therefore always reads as ppid 0.
pub fn die_with_parent_unchecked() -> SysResult<()> {
    // SAFETY: plain prctl call.
    check(unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong, 0, 0, 0) }, "prctl(PDEATHSIG)")
}

pub fn pipe_cloexec() -> SysResult<(libc::c_int, libc::c_int)> {
    let mut fds = [0; 2];
    // SAFETY: fds is a valid 2-element buffer.
    check(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, "pipe2")?;
    Ok((fds[0], fds[1]))
}

pub fn close(fd: libc::c_int) {
    // SAFETY: closing an fd we own; errors are irrelevant here.
    unsafe {
        libc::close(fd);
    }
}

pub fn write_all(fd: libc::c_int, mut buf: &[u8]) {
    while !buf.is_empty() {
        // SAFETY: buf is a valid slice.
        let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
        if n <= 0 {
            return;
        }
        buf = &buf[n as usize..];
    }
}

/// Read until EOF (bounded).
pub fn read_to_end(fd: libc::c_int, limit: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        // SAFETY: buf is a valid buffer.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            break;
        }
        if out.len() < limit {
            out.extend_from_slice(&buf[..n as usize]);
        }
    }
    out
}

pub fn read_byte(fd: libc::c_int) -> Option<u8> {
    let mut b = [0u8; 1];
    // SAFETY: b is a valid 1-byte buffer.
    let n = unsafe { libc::read(fd, b.as_mut_ptr().cast(), 1) };
    (n == 1).then_some(b[0])
}

pub fn fork() -> SysResult<libc::pid_t> {
    // SAFETY: the helper is single-threaded whenever it forks.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(last("fork"));
    }
    Ok(pid)
}

/// Wait for `pid`; map the status to a shell-style exit code.
pub fn wait_exit_code(pid: libc::pid_t) -> i32 {
    let mut status = 0;
    loop {
        // SAFETY: status is a valid out-pointer.
        let rc = unsafe { libc::waitpid(pid, &mut status, 0) };
        if rc == pid {
            break;
        }
        if rc < 0 && Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return 1;
        }
    }
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        1
    }
}

pub fn mount(src: Option<&Path>, dst: &Path, fstype: Option<&str>, flags: libc::c_ulong, data: Option<&str>) -> SysResult<()> {
    let src_c = src.map(cstr).transpose()?;
    let dst_c = cstr(dst)?;
    let fs_c = fstype.map(|f| CString::new(f).unwrap());
    let data_c = data.map(|d| CString::new(d).unwrap());
    // SAFETY: all pointers are either null or valid C strings that outlive the call.
    let rc = unsafe {
        libc::mount(
            src_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            dst_c.as_ptr(),
            fs_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            flags,
            data_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr().cast()),
        )
    };
    check(rc, &format!("mount {}", dst.display()))
}

/// Mount flags that the kernel locks on a bind mount made inside a user
/// namespace; a remount must repeat them or it fails with EPERM.
pub fn locked_flags(p: &Path) -> libc::c_ulong {
    let Ok(c) = cstr(p) else { return 0 };
    // SAFETY: zeroed statvfs is a valid out-parameter.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: c is a valid C string, st a valid out-pointer.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return 0;
    }
    let f = st.f_flag;
    let mut out = 0;
    for (st_flag, ms_flag) in [
        (libc::ST_NOSUID, libc::MS_NOSUID),
        (libc::ST_NODEV, libc::MS_NODEV),
        (libc::ST_NOEXEC, libc::MS_NOEXEC),
        (libc::ST_NOATIME, libc::MS_NOATIME),
        (libc::ST_NODIRATIME, libc::MS_NODIRATIME),
        (libc::ST_RELATIME, libc::MS_RELATIME),
    ] {
        if f & st_flag != 0 {
            out |= ms_flag;
        }
    }
    out
}
