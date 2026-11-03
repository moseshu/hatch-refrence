//! Real-process isolation tests. Each test first probes the host; when the
//! host refuses user namespaces the test prints why and returns (skip) rather
//! than pretending to pass.

use hatch_sandbox::{HelperCmd, Limits, Sandbox, SandboxMode};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn sandbox() -> Sandbox {
    Sandbox {
        mode: SandboxMode::Namespaces,
        helper: Some(HelperCmd { program: PathBuf::from(env!("CARGO_BIN_EXE_hatch-sandbox-helper")), prefix_args: vec![] }),
        limits: Limits { cpu_secs: 20, ..Limits::default() },
        max_output: 64 * 1024,
    }
}

/// `Some(sandbox)` when the host supports it, else prints the skip reason.
fn usable(test: &str) -> Option<Sandbox> {
    let s = sandbox();
    match s.probe() {
        Ok(_) => Some(s),
        Err(e) => {
            eprintln!("SKIP {test}: sandbox unavailable on this host: {e}");
            None
        }
    }
}

fn workspace() -> (tempfile::TempDir, PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().canonicalize().unwrap();
    (d, p)
}

fn sh(s: &Sandbox, ws: &Path, cmd: &str) -> hatch_sandbox::RunOutput {
    s.run_shell(ws, ws, cmd, Duration::from_secs(20)).unwrap()
}

#[test]
fn probe_reports_full_isolation() {
    let Some(s) = usable("probe_reports_full_isolation") else { return };
    let r = s.probe().unwrap();
    eprintln!("isolation: {}", r.summary());
    assert_eq!(r.uid, 65534);
    assert!(r.user_ns && r.mount_ns && r.net_ns && r.pid_ns && r.pivot_root);
    assert!(r.caps_dropped && r.no_new_privs && r.seccomp && r.rlimits);
}

#[test]
fn runs_as_nobody_with_clean_env_and_writable_workspace() {
    let Some(s) = usable("runs_as_nobody") else { return };
    let (_d, ws) = workspace();
    std::env::set_var("HATCH_TEST_SECRET_TOKEN", "sk-should-not-leak-123456");
    let out = sh(&s, &ws, "id -u; env; echo hello > made.txt");
    assert_eq!(out.exit_code, Some(0), "{out:?}");
    let lines: Vec<&str> = out.stdout.lines().collect();
    assert_eq!(lines[0], "65534");
    assert!(!out.stdout.contains("HATCH_TEST_SECRET_TOKEN"), "caller env leaked: {}", out.stdout);
    assert!(out.stdout.contains(&format!("HOME={}", ws.display())));
    assert_eq!(std::fs::read_to_string(ws.join("made.txt")).unwrap(), "hello\n");
    assert!(out.isolated);
}

#[test]
fn files_outside_the_workspace_do_not_exist_inside() {
    let Some(s) = usable("files_outside") else { return };
    let (_d, ws) = workspace();
    let (_o, outside) = workspace();
    std::fs::write(outside.join("secret.txt"), "CANARY-OUTSIDE-42").unwrap();
    let out = sh(&s, &ws, &format!("cat {}/secret.txt; cat /etc/shadow; ls /root; ls /home", outside.display()));
    assert!(!out.stdout.contains("CANARY-OUTSIDE-42"));
    assert!(out.stderr.contains("No such file"), "{out:?}");
    // System files are there but read-only.
    let out = sh(&s, &ws, "test -x /bin/sh && echo sh-ok; touch /usr/x 2>&1; touch /etc/x 2>&1; touch /x 2>&1");
    assert!(out.stdout.contains("sh-ok"));
    assert_eq!(out.stdout.matches("Read-only file system").count(), 3, "{}", out.stdout);
}

#[test]
fn no_network() {
    let Some(s) = usable("no_network") else { return };
    let (_d, ws) = workspace();
    let probe = if Path::new("/usr/bin/bash").exists() || Path::new("/bin/bash").exists() {
        "bash -c 'echo > /dev/tcp/1.1.1.1/53' && echo CONNECTED || echo BLOCKED"
    } else {
        eprintln!("SKIP no_network: bash not available");
        return;
    };
    let out = sh(&s, &ws, probe);
    assert!(out.stdout.contains("BLOCKED"), "{out:?}");
    assert!(!out.stdout.contains("CONNECTED"));
}

#[test]
fn seccomp_and_capabilities() {
    let Some(s) = usable("seccomp") else { return };
    let (_d, ws) = workspace();
    let out = sh(&s, &ws, "grep -E '^(Seccomp|NoNewPrivs|CapEff|CapBnd):' /proc/self/status; unshare -U true && echo UNSHARE-OK || echo UNSHARE-DENIED");
    assert!(out.stdout.contains("Seccomp:\t2"), "{}", out.stdout);
    assert!(out.stdout.contains("NoNewPrivs:\t1"));
    assert!(out.stdout.contains("CapEff:\t0000000000000000"));
    assert!(out.stdout.contains("CapBnd:\t0000000000000000"));
    assert!(out.stdout.contains("UNSHARE-DENIED"));
}

#[test]
fn timeout_kills_the_whole_tree() {
    let Some(s) = usable("timeout") else { return };
    let (_d, ws) = workspace();
    let started = Instant::now();
    // A background child that would outlive a naive kill.
    let out = s.run_shell(&ws, &ws, "(sleep 30; touch late.txt) & sleep 30", Duration::from_millis(500)).unwrap();
    assert!(out.timed_out);
    assert!(started.elapsed() < Duration::from_secs(5));
    std::thread::sleep(Duration::from_millis(300));
    assert!(!ws.join("late.txt").exists());
    assert!(no_process_with_arg("30"), "sleep 30 survived the kill");
}

/// True when no live process runs `sleep <arg>` (scan /proc on the host).
fn no_process_with_arg(arg: &str) -> bool {
    std::fs::read_dir("/proc").unwrap().flatten().all(|e| {
        let cmd = std::fs::read(e.path().join("cmdline")).unwrap_or_default();
        let parts: Vec<&[u8]> = cmd.split(|b| *b == 0).collect();
        !(parts.len() >= 2 && parts[0].ends_with(b"sleep") && parts[1] == arg.as_bytes())
    })
}

#[test]
fn dies_with_its_parent() {
    let Some(s) = usable("dies_with_parent") else { return };
    let (_d, ws) = workspace();
    let helper = env!("CARGO_BIN_EXE_hatch-sandbox-helper");
    let spec = serde_json::json!({
        "argv": ["/bin/sh", "-c", "sleep 31.25"],
        "workspace": ws, "cwd": ws, "env": [["PATH", "/usr/bin:/bin"]],
        "limits": s.limits,
    });
    // An intermediate shell starts the helper in the background, then exits:
    // the helper's parent is gone, so the whole chain must die.
    let script = format!("'{helper}' --spec '{spec}' & sleep 0.7; exit 0");
    let st = std::process::Command::new("/bin/sh").arg("-c").arg(script).status().unwrap();
    assert!(st.success());
    std::thread::sleep(Duration::from_millis(500));
    assert!(no_process_with_arg("31.25"), "sandboxed process outlived its parent");
}

#[test]
fn output_is_capped() {
    let Some(mut s) = usable("output_is_capped") else { return };
    s.max_output = 1000;
    let (_d, ws) = workspace();
    let out = sh(&s, &ws, "head -c 100000 /dev/zero | tr '\\0' 'a'");
    assert_eq!(out.stdout.len(), 1000);
    assert!(out.stdout_truncated);
    assert_eq!(out.exit_code, Some(0));
}

#[test]
fn mode_none_is_labelled_unisolated() {
    let (_d, ws) = workspace();
    let s = Sandbox { mode: SandboxMode::None, helper: None, limits: Limits::default(), max_output: 4096 };
    let out = s.run_shell(&ws, &ws, "echo hi", Duration::from_secs(5)).unwrap();
    assert_eq!(out.stdout, "hi\n");
    assert!(!out.isolated);
    assert!(out.render(100).contains("未隔离"));
    assert!(s.probe().is_err());
}

#[test]
fn cwd_outside_workspace_is_refused() {
    let (_d, ws) = workspace();
    let s = sandbox();
    assert!(s.run_shell(&ws, Path::new("/"), "true", Duration::from_secs(5)).is_err());
}

#[test]
fn cancel_kills_the_whole_tree_promptly() {
    let Some(s) = usable("cancel_kills_the_whole_tree_promptly") else { return };
    let (_d, ws) = workspace();
    let token = hatch_core::CancelToken::new();
    let t2 = token.clone();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        t2.cancel("Ctrl-C");
    });
    let started = Instant::now();
    let out = s
        .run_shell_with(&ws, &ws, "sleep 47.25 & sleep 47.25; echo never", Duration::from_secs(60), Some(&token))
        .unwrap();
    canceller.join().unwrap();
    assert!(out.cancelled && !out.timed_out, "{out:?}");
    assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
    assert!(!out.stdout.contains("never"));
    assert!(out.render(100).contains("已取消"));
    let deadline = Instant::now() + Duration::from_secs(3);
    while !no_process_with_arg("47.25") && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(no_process_with_arg("47.25"), "sandboxed processes survived cancellation");
    // A token cancelled up-front refuses to start anything.
    assert!(matches!(
        s.run_shell_with(&ws, &ws, "echo hi", Duration::from_secs(5), Some(&token)),
        Err(hatch_core::Error::Cancelled(_))
    ));
}
