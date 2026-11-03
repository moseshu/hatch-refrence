//! A small seccomp-BPF deny-list.
//!
//! The filter is a deny-list on purpose: the sandbox runs ordinary developer
//! commands (shells, compilers, test runners) whose syscall needs are broad,
//! while the dangerous surface is narrow and well known. Denied calls fail with
//! `EPERM` (or `EACCES` for IP sockets) instead of killing the process, so tools
//! degrade with a readable error. A foreign architecture or the x32 ABI kills
//! the process outright.

use libc::{c_long, sock_filter};

const BPF_LD_W_ABS: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
const BPF_JEQ_K: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
const BPF_JGE_K: u16 = 0x35; // BPF_JMP | BPF_JGE | BPF_K
const BPF_JSET_K: u16 = 0x45; // BPF_JMP | BPF_JSET | BPF_K
const BPF_RET_K: u16 = 0x06; // BPF_RET | BPF_K

const RET_KILL_PROCESS: u32 = 0x8000_0000;
const RET_ERRNO: u32 = 0x0005_0000;
const RET_ALLOW: u32 = 0x7fff_0000;

// Offsets into `struct seccomp_data`.
const OFF_NR: u32 = 0;
const OFF_ARCH: u32 = 4;
const OFF_ARG0: u32 = 16; // low 32 bits on little-endian targets

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xC000_003E;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xC000_00B7;

/// Namespace-creating clone flags a sandboxed process may not use.
const NS_FLAGS: u32 = (libc::CLONE_NEWUSER
    | libc::CLONE_NEWNS
    | libc::CLONE_NEWPID
    | libc::CLONE_NEWNET
    | libc::CLONE_NEWIPC
    | libc::CLONE_NEWUTS
    | libc::CLONE_NEWCGROUP) as u32;

/// Syscalls that fail with EPERM inside the sandbox.
pub fn denied_syscalls() -> Vec<(&'static str, c_long)> {
    vec![
        ("ptrace", libc::SYS_ptrace),
        ("process_vm_readv", libc::SYS_process_vm_readv),
        ("process_vm_writev", libc::SYS_process_vm_writev),
        ("mount", libc::SYS_mount),
        ("umount2", libc::SYS_umount2),
        ("pivot_root", libc::SYS_pivot_root),
        ("chroot", libc::SYS_chroot),
        ("fsopen", libc::SYS_fsopen),
        ("fsmount", libc::SYS_fsmount),
        ("move_mount", libc::SYS_move_mount),
        ("open_tree", libc::SYS_open_tree),
        ("mount_setattr", libc::SYS_mount_setattr),
        ("unshare", libc::SYS_unshare),
        ("setns", libc::SYS_setns),
        ("reboot", libc::SYS_reboot),
        ("kexec_load", libc::SYS_kexec_load),
        ("kexec_file_load", libc::SYS_kexec_file_load),
        ("init_module", libc::SYS_init_module),
        ("finit_module", libc::SYS_finit_module),
        ("delete_module", libc::SYS_delete_module),
        ("bpf", libc::SYS_bpf),
        ("perf_event_open", libc::SYS_perf_event_open),
        ("userfaultfd", libc::SYS_userfaultfd),
        ("keyctl", libc::SYS_keyctl),
        ("add_key", libc::SYS_add_key),
        ("request_key", libc::SYS_request_key),
        ("open_by_handle_at", libc::SYS_open_by_handle_at),
        ("swapon", libc::SYS_swapon),
        ("swapoff", libc::SYS_swapoff),
        ("acct", libc::SYS_acct),
        ("quotactl", libc::SYS_quotactl),
        ("syslog", libc::SYS_syslog),
        ("settimeofday", libc::SYS_settimeofday),
        ("clock_settime", libc::SYS_clock_settime),
        ("clock_adjtime", libc::SYS_clock_adjtime),
    ]
}

fn stmt(code: u16, k: u32) -> sock_filter {
    sock_filter { code, jt: 0, jf: 0, k }
}

fn jump(code: u16, k: u32, jt: u8, jf: u8) -> sock_filter {
    sock_filter { code, jt, jf, k }
}

/// Build the BPF program.
pub fn build_filter() -> Vec<sock_filter> {
    let errno = |e: i32| RET_ERRNO | (e as u32 & 0xffff);
    let mut f = vec![
        stmt(BPF_LD_W_ABS, OFF_ARCH),
        jump(BPF_JEQ_K, AUDIT_ARCH, 1, 0),
        stmt(BPF_RET_K, RET_KILL_PROCESS),
        stmt(BPF_LD_W_ABS, OFF_NR),
    ];
    #[cfg(target_arch = "x86_64")]
    {
        // x32 ABI syscalls carry bit 30.
        f.push(jump(BPF_JGE_K, 0x4000_0000, 0, 1));
        f.push(stmt(BPF_RET_K, RET_KILL_PROCESS));
    }
    for (_, nr) in denied_syscalls() {
        f.push(jump(BPF_JEQ_K, nr as u32, 0, 1));
        f.push(stmt(BPF_RET_K, errno(libc::EPERM)));
    }
    // clone3 passes flags in memory we cannot inspect: make libc fall back to clone.
    f.push(jump(BPF_JEQ_K, libc::SYS_clone3 as u32, 0, 1));
    f.push(stmt(BPF_RET_K, errno(libc::ENOSYS)));
    // clone: refuse namespace flags.
    f.push(jump(BPF_JEQ_K, libc::SYS_clone as u32, 0, 4));
    f.push(stmt(BPF_LD_W_ABS, OFF_ARG0));
    f.push(jump(BPF_JSET_K, NS_FLAGS, 0, 1));
    f.push(stmt(BPF_RET_K, errno(libc::EPERM)));
    f.push(stmt(BPF_RET_K, RET_ALLOW));
    // socket: refuse IPv4 / IPv6 (AF_UNIX and friends stay available).
    f.push(jump(BPF_JEQ_K, libc::SYS_socket as u32, 0, 5));
    f.push(stmt(BPF_LD_W_ABS, OFF_ARG0));
    f.push(jump(BPF_JEQ_K, libc::AF_INET as u32, 2, 0));
    f.push(jump(BPF_JEQ_K, libc::AF_INET6 as u32, 1, 0));
    f.push(stmt(BPF_RET_K, RET_ALLOW));
    f.push(stmt(BPF_RET_K, errno(libc::EACCES)));
    f.push(stmt(BPF_RET_K, RET_ALLOW));
    f
}

/// Install the filter on the calling thread. Requires `no_new_privs` (or
/// CAP_SYS_ADMIN). Irreversible.
pub fn install() -> Result<(), String> {
    let filter = build_filter();
    let prog = libc::sock_fprog { len: filter.len() as u16, filter: filter.as_ptr() as *mut sock_filter };
    // SAFETY: `prog` points at a valid, live filter for the duration of the call.
    let rc = unsafe { libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &prog as *const libc::sock_fprog) };
    if rc != 0 {
        return Err(format!("seccomp: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_shape() {
        let f = build_filter();
        assert!(f.len() < 255, "jump offsets are u8");
        assert_eq!(f[0].code, BPF_LD_W_ABS);
        assert_eq!(f[0].k, OFF_ARCH);
        assert_eq!(f.last().unwrap().k, RET_ALLOW);
        // Every denied syscall has its own compare + errno return.
        let compares = f.iter().filter(|i| i.code == BPF_JEQ_K && i.jt == 0 && i.jf == 1).count();
        assert!(compares >= denied_syscalls().len());
        assert!(denied_syscalls().iter().any(|(n, _)| *n == "ptrace"));
    }
}
