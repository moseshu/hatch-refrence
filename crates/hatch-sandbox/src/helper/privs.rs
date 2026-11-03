//! Resource limits and privilege drop, applied just before `execve`.

use super::sys::{self, SysResult};
use crate::spec::Limits;

fn setrlimit(resource: libc::__rlimit_resource_t, value: u64) -> SysResult<()> {
    let lim = libc::rlimit { rlim_cur: value as libc::rlim_t, rlim_max: value as libc::rlim_t };
    // SAFETY: lim is a valid rlimit.
    sys::check(unsafe { libc::setrlimit(resource, &lim) }, "setrlimit")
}

pub fn apply_limits(l: &Limits) -> SysResult<()> {
    const MIB: u64 = 1024 * 1024;
    setrlimit(libc::RLIMIT_CPU, l.cpu_secs.max(1))?;
    setrlimit(libc::RLIMIT_AS, l.memory_mb.max(64) * MIB)?;
    setrlimit(libc::RLIMIT_FSIZE, l.file_size_mb.max(1) * MIB)?;
    setrlimit(libc::RLIMIT_NPROC, l.max_processes.max(16))?;
    setrlimit(libc::RLIMIT_NOFILE, l.max_open_files.max(16))?;
    setrlimit(libc::RLIMIT_CORE, 0)
}

/// Highest capability number known to the running kernel.
pub fn last_cap() -> u32 {
    std::fs::read_to_string("/proc/sys/kernel/cap_last_cap").ok().and_then(|s| s.trim().parse().ok()).unwrap_or(40)
}

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: libc::c_int,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

/// Empty the bounding, ambient, effective, permitted and inheritable sets and
/// set `no_new_privs`, so nothing exec'd later can regain privileges.
pub fn drop_all(last_cap: u32) -> SysResult<()> {
    // SAFETY: prctl/capset with valid arguments.
    unsafe {
        for cap in 0..=last_cap {
            if libc::prctl(libc::PR_CAPBSET_DROP, cap as libc::c_ulong, 0, 0, 0) != 0
                && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINVAL)
            {
                return Err(sys::last("prctl(CAPBSET_DROP)"));
            }
        }
        sys::check(libc::prctl(libc::PR_CAP_AMBIENT, libc::PR_CAP_AMBIENT_CLEAR_ALL as libc::c_ulong, 0, 0, 0), "prctl(AMBIENT_CLEAR_ALL)")?;
        sys::check(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), "prctl(NO_NEW_PRIVS)")?;
        let hdr = CapHeader { version: 0x2008_0522, pid: 0 };
        let data = [CapData::default(); 2];
        if libc::syscall(libc::SYS_capset, &hdr as *const CapHeader, data.as_ptr()) != 0 {
            return Err(sys::last("capset"));
        }
    }
    Ok(())
}
