//! Spawn a child process with extra env vars, capture its output, kill
//! its process group on timeout. Shared by the helper's `inject` op.
//!
//! The caller's secret values never appear in the error message.

use std::io::Write;

/// What a spawned child produced. Shared by the helper's `inject` op.
#[derive(Clone, Debug)]
pub struct SpawnOutcome {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
}

/// Spawn `command` with `extra_env` added to the child's environment,
/// write `stdin_text` to its stdin, and capture stdout/stderr. On timeout
/// the whole process group is killed (unix) and an error is returned.
/// The caller's values never appear in the error message.
pub(crate) fn spawn_capture(
    command: &str,
    args: &[String],
    extra_env: &[(String, String)],
    stdin_text: &str,
    timeout: std::time::Duration,
) -> std::result::Result<SpawnOutcome, String> {
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    #[cfg(unix)]
    use std::os::unix::process::CommandExt;
    use std::sync::mpsc;

    let mut cmd = Command::new(command);
    cmd.args(args)
        .envs(extra_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child: Child =
        cmd.spawn().map_err(|e| format!("failed to spawn `{command}`: {e}"))?;
    let pid = child.id();
    let stdin_text = stdin_text.to_string();
    let mut stdin = child.stdin.take();
    std::thread::spawn(move || {
        if let Some(mut s) = stdin.take() {
            let _ = s.write_all(stdin_text.as_bytes());
            let _ = s.flush();
        }
    });
    let (out_tx, out_rx) = mpsc::channel::<String>();
    if let Some(out) = child.stdout.take() {
        std::thread::spawn(move || {
            let mut r = BufReader::new(out);
            let mut buf = String::new();
            while r.read_line(&mut buf).map(|n| n > 0).unwrap_or(false) {
                let _ = out_tx.send(std::mem::take(&mut buf));
            }
        });
    }
    let (err_tx, err_rx) = mpsc::channel::<String>();
    if let Some(err) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut r = BufReader::new(err);
            let mut buf = String::new();
            while r.read_line(&mut buf).map(|n| n > 0).unwrap_or(false) {
                let _ = err_tx.send(std::mem::take(&mut buf));
            }
        });
    }
    // First stdout line (or EOF) within the timeout.
    match out_rx.recv_timeout(timeout) {
        Ok(first) => {
            let mut stdout = first;
            while let Ok(l) = out_rx.recv_timeout(std::time::Duration::from_millis(200)) {
                stdout.push_str(&l);
            }
            let mut stderr = String::new();
            while let Ok(l) = err_rx.try_recv() {
                stderr.push_str(&l);
            }
            let exit_code = wait_child(&mut child, pid);
            Ok(SpawnOutcome { stdout, stderr, exit_code })
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            kill_group(pid);
            let _ = child.wait();
            Err(format!("child `{command}` timed out after {}s", timeout.as_secs()))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            // EOF with no output: still wait for exit, then report empty.
            let mut stderr = String::new();
            while let Ok(l) = err_rx.try_recv() {
                stderr.push_str(&l);
            }
            let exit_code = wait_child(&mut child, pid);
            Ok(SpawnOutcome { stdout: String::new(), stderr, exit_code })
        }
    }
}

fn wait_child(child: &mut std::process::Child, pid: u32) -> Option<i32> {
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code(),
            Ok(None) => {
                if start.elapsed() >= std::time::Duration::from_secs(5) {
                    kill_group(pid);
                    return child.wait().ok().and_then(|s| s.code());
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
}

fn kill_group(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
    }
}

