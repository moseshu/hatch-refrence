//! The sandbox's filesystem view.
//!
//! A fresh tmpfs becomes the new root. Into it go: read-only binds of the
//! system directories (`/usr`, `/bin`, `/lib*`, `/sbin`), a short allow-list of
//! `/etc` entries (no `shadow`, no `sudoers`, no ssh host keys), a handful of
//! device nodes, a private `/tmp`, `/proc` of the new pid namespace when the
//! host allows it, and the workspace — the only writable host path — at its
//! original absolute path. Everything else (home directories, `/root`, `/var`,
//! other users' files) simply does not exist inside.

use super::sys::{self, mount, SysResult};
use crate::spec::IsolationReport;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

const SYSTEM_DIRS: &[&str] = &["/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32"];

const ETC_ALLOW: &[&str] = &[
    "passwd",
    "group",
    "hosts",
    "resolv.conf",
    "nsswitch.conf",
    "host.conf",
    "ld.so.cache",
    "ld.so.conf",
    "ld.so.conf.d",
    "ssl",
    "ca-certificates",
    "ca-certificates.conf",
    "pki",
    "localtime",
    "timezone",
    "alternatives",
    "os-release",
    "lsb-release",
    "mime.types",
    "protocols",
    "services",
    "gai.conf",
    "locale.alias",
];

const DEVICES: &[&str] = &["null", "zero", "full", "random", "urandom"];

fn bind_readonly(src: &Path, dst: &Path) -> SysResult<()> {
    mount(Some(src), dst, None, libc::MS_BIND | libc::MS_REC, None)?;
    let flags = libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | sys::locked_flags(src);
    mount(None, dst, None, flags, None)
}

/// Mirror `src` into the new root: symlinks are recreated, directories and
/// files bind-mounted read-only.
fn mirror_readonly(src: &Path, newroot: &Path) -> SysResult<()> {
    let Ok(meta) = fs::symlink_metadata(src) else { return Ok(()) };
    let dst = newroot.join(src.strip_prefix("/").unwrap_or(src));
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    if meta.file_type().is_symlink() {
        let target = fs::read_link(src).map_err(|e| format!("readlink {}: {e}", src.display()))?;
        return symlink(&target, &dst).map_err(|e| format!("symlink {}: {e}", dst.display()));
    }
    if meta.is_dir() {
        fs::create_dir_all(&dst).map_err(|e| format!("mkdir {}: {e}", dst.display()))?;
    } else {
        fs::write(&dst, b"").map_err(|e| format!("touch {}: {e}", dst.display()))?;
    }
    bind_readonly(src, &dst)
}

fn bind_device(name: &str, newroot: &Path) -> SysResult<()> {
    let src = Path::new("/dev").join(name);
    if !src.exists() {
        return Ok(());
    }
    let dst = newroot.join("dev").join(name);
    fs::write(&dst, b"").map_err(|e| format!("touch {}: {e}", dst.display()))?;
    mount(Some(&src), &dst, None, libc::MS_BIND, None)
}

fn pivot(newroot: &Path) -> SysResult<()> {
    std::env::set_current_dir(newroot).map_err(|e| format!("chdir newroot: {e}"))?;
    let dot = sys::cstr(Path::new("."))?;
    // SAFETY: pivot_root(".", ".") stacks the old root on top of the new one.
    let rc = unsafe { libc::syscall(libc::SYS_pivot_root, dot.as_ptr(), dot.as_ptr()) };
    if rc != 0 {
        return Err(sys::last("pivot_root"));
    }
    // SAFETY: detach the old root, which now sits on ".".
    sys::check(unsafe { libc::umount2(dot.as_ptr(), libc::MNT_DETACH) }, "umount old root")?;
    std::env::set_current_dir("/").map_err(|e| format!("chdir /: {e}"))
}

/// Build the new root in `staging` and pivot into it.
pub fn setup(workspace: &Path, staging: &Path, pid_ns: bool, report: &mut IsolationReport) -> SysResult<()> {
    mount(None, Path::new("/"), None, libc::MS_REC | libc::MS_PRIVATE, None)?;
    mount(Some(Path::new("tmpfs")), staging, Some("tmpfs"), libc::MS_NOSUID | libc::MS_NODEV, Some("mode=0755,size=16m"))?;
    let newroot: PathBuf = staging.to_path_buf();
    for dir in SYSTEM_DIRS {
        mirror_readonly(Path::new(dir), &newroot)?;
    }
    fs::create_dir_all(newroot.join("etc")).map_err(|e| format!("mkdir etc: {e}"))?;
    for name in ETC_ALLOW {
        mirror_readonly(&Path::new("/etc").join(name), &newroot)?;
    }
    fs::create_dir_all(newroot.join("dev")).map_err(|e| format!("mkdir dev: {e}"))?;
    for d in DEVICES {
        bind_device(d, &newroot)?;
    }
    for (link, target) in [("dev/fd", "/proc/self/fd"), ("dev/stdin", "/proc/self/fd/0"), ("dev/stdout", "/proc/self/fd/1"), ("dev/stderr", "/proc/self/fd/2")] {
        let _ = symlink(target, newroot.join(link));
    }
    let tmp = newroot.join("tmp");
    fs::create_dir_all(&tmp).map_err(|e| format!("mkdir tmp: {e}"))?;
    mount(Some(Path::new("tmpfs")), &tmp, Some("tmpfs"), libc::MS_NOSUID | libc::MS_NODEV, Some("mode=1777,size=256m"))?;

    // The workspace: writable, at the same absolute path as on the host.
    let ws_dst = newroot.join(workspace.strip_prefix("/").map_err(|_| "workspace must be absolute".to_string())?);
    fs::create_dir_all(&ws_dst).map_err(|e| format!("mkdir workspace: {e}"))?;
    mount(Some(workspace), &ws_dst, None, libc::MS_BIND | libc::MS_REC, None)?;
    let ws_flags = libc::MS_BIND | libc::MS_REMOUNT | libc::MS_NOSUID | libc::MS_NODEV | sys::locked_flags(workspace);
    mount(None, &ws_dst, None, ws_flags, None)?;

    let proc_dir = newroot.join("proc");
    fs::create_dir_all(&proc_dir).map_err(|e| format!("mkdir proc: {e}"))?;
    if pid_ns {
        match mount(Some(Path::new("proc")), &proc_dir, Some("proc"), libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, None) {
            Ok(()) => report.proc_mounted = true,
            Err(e) => report.notes.push(format!("/proc 未挂载（宿主不允许）：{e}")),
        }
    }
    pivot(&newroot)?;
    report.pivot_root = true;
    // Freeze the skeleton: the tmpfs root itself becomes read-only.
    match mount(None, Path::new("/"), None, libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV, None) {
        Ok(()) => report.readonly_root = true,
        Err(e) => report.notes.push(format!("根目录未能设为只读：{e}")),
    }
    Ok(())
}
