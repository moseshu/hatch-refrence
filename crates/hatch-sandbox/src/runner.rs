//! Caller side: spawn the helper, enforce the timeout, collect bounded output.

use crate::spec::{IsolationReport, Limits, Spec, REPORT_MARKER, SETUP_ERROR_EXIT, SETUP_ERROR_MARKER};
use hatch_core::{Error, Result};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How commands are isolated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    /// Namespaces + pivot_root + seccomp + capability drop (the default).
    Namespaces,
    /// No isolation at all. Only when the user explicitly configures it;
    /// every result is labelled "未隔离".
    None,
}

impl SandboxMode {
    pub fn parse(s: &str) -> Result<SandboxMode> {
        match s.trim() {
            "" | "namespaces" | "ns" => Ok(SandboxMode::Namespaces),
            "none" | "off" => Ok(SandboxMode::None),
            other => Err(Error::Invalid(format!("unknown sandbox mode {other:?} (namespaces | none)"))),
        }
    }
}

/// How to start the helper: a program plus leading arguments
/// (e.g. the `hatch` binary with its hidden `__sandbox-helper` subcommand).
#[derive(Clone, Debug)]
pub struct HelperCmd {
    pub program: PathBuf,
    pub prefix_args: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Sandbox {
    pub mode: SandboxMode,
    pub helper: Option<HelperCmd>,
    pub limits: Limits,
    /// Per-stream output cap in bytes; the rest is drained and dropped.
    pub max_output: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RunOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub timed_out: bool,
    /// False only in `SandboxMode::None`.
    pub isolated: bool,
    pub report: Option<IsolationReport>,
    pub duration_ms: u64,
}

impl RunOutput {
    /// Compact text for a tool result.
    pub fn render(&self, cap: usize) -> String {
        let status = if self.timed_out {
            "超时被终止".to_string()
        } else {
            format!("exit={}", self.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "?".into()))
        };
        let iso = if self.isolated { "沙箱隔离" } else { "未隔离" };
        let cut = |s: &str| -> String {
            if s.chars().count() > cap {
                format!("{}…（已截断）", s.chars().take(cap).collect::<String>())
            } else {
                s.to_string()
            }
        };
        let mut out = format!("{status}（{iso}，{} ms）\n", self.duration_ms);
        if !self.stdout.is_empty() {
            out.push_str(&format!("--- stdout{} ---\n{}\n", if self.stdout_truncated { "（截断）" } else { "" }, cut(&self.stdout)));
        }
        if !self.stderr.is_empty() {
            out.push_str(&format!("--- stderr{} ---\n{}\n", if self.stderr_truncated { "（截断）" } else { "" }, cut(&self.stderr)));
        }
        out
    }
}

/// Environment a sandboxed command receives — nothing from the caller leaks.
pub fn clean_env(workspace: &Path) -> Vec<(String, String)> {
    vec![
        ("PATH".into(), "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into()),
        ("LANG".into(), "C.UTF-8".into()),
        ("LC_ALL".into(), "C.UTF-8".into()),
        ("HOME".into(), workspace.display().to_string()),
        ("TMPDIR".into(), "/tmp".into()),
        ("TERM".into(), "dumb".into()),
    ]
}

fn reader<R: Read + Send + 'static>(mut r: R, cap: usize) -> JoinHandle<(Vec<u8>, bool)> {
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        let mut truncated = false;
        while let Ok(n) = r.read(&mut buf) {
            if n == 0 {
                break;
            }
            let room = cap.saturating_sub(out.len());
            out.extend_from_slice(&buf[..n.min(room)]);
            truncated |= n > room;
        }
        (out, truncated)
    })
}

fn kill_group(child: &Child) {
    // SAFETY: signalling the process group we created (pgid == child pid).
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
        libc::kill(child.id() as libc::pid_t, libc::SIGKILL);
    }
}

impl Sandbox {
    /// Run `sh -c <command>` with `cwd` (absolute, inside `workspace`).
    pub fn run_shell(&self, workspace: &Path, cwd: &Path, command: &str, timeout: Duration) -> Result<RunOutput> {
        self.run(workspace, cwd, &["/bin/sh".into(), "-c".into(), command.into()], timeout, false)
    }

    pub fn run(&self, workspace: &Path, cwd: &Path, argv: &[String], timeout: Duration, report: bool) -> Result<RunOutput> {
        if !cwd.starts_with(workspace) {
            return Err(Error::Denied(format!("working directory {} is outside the workspace", cwd.display())));
        }
        let mut cmd = match self.mode {
            SandboxMode::Namespaces => {
                let h = self.helper.as_ref().ok_or_else(|| Error::Sandbox("沙箱不可用：没有配置 helper 程序".into()))?;
                let spec = Spec {
                    argv: argv.to_vec(),
                    workspace: workspace.to_path_buf(),
                    cwd: cwd.to_path_buf(),
                    env: clean_env(workspace),
                    network: false,
                    limits: self.limits.clone(),
                    report,
                };
                let mut c = Command::new(&h.program);
                c.args(&h.prefix_args).arg("--spec").arg(serde_json::to_string(&spec)?).env_clear();
                c
            }
            SandboxMode::None => {
                let (prog, args) = argv.split_first().ok_or_else(|| Error::Invalid("empty command".into()))?;
                let mut c = Command::new(prog);
                c.args(args).env_clear().envs(clean_env(workspace)).current_dir(cwd);
                c
            }
        };
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
        let started = Instant::now();
        let mut child = cmd.spawn().map_err(|e| Error::Sandbox(format!("无法启动沙箱：{e}")))?;
        let out_t = reader(child.stdout.take().expect("piped"), self.max_output);
        let err_t = reader(child.stderr.take().expect("piped"), self.max_output + 8192);
        let mut timed_out = false;
        let status = loop {
            if let Some(s) = child.try_wait()? {
                break s;
            }
            if started.elapsed() >= timeout {
                timed_out = true;
                kill_group(&child);
                break child.wait()?;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let (stdout, stdout_truncated) = out_t.join().unwrap_or_default();
        let (stderr_raw, mut stderr_truncated) = err_t.join().unwrap_or_default();
        let mut report_parsed = None;
        let mut setup_error = None;
        let mut stderr = String::new();
        for line in String::from_utf8_lossy(&stderr_raw).split_inclusive('\n') {
            if let Some(j) = line.strip_prefix(REPORT_MARKER) {
                report_parsed = serde_json::from_str(j.trim()).ok();
            } else if let Some(e) = line.strip_prefix(SETUP_ERROR_MARKER) {
                setup_error = Some(e.trim().to_string());
            } else {
                stderr.push_str(line);
            }
        }
        if self.mode == SandboxMode::Namespaces && status.code() == Some(SETUP_ERROR_EXIT) {
            if let Some(e) = setup_error {
                return Err(Error::Sandbox(format!("沙箱不可用，命令未执行：{e}")));
            }
        }
        if stderr.len() > self.max_output {
            let mut cut = self.max_output;
            while !stderr.is_char_boundary(cut) {
                cut -= 1;
            }
            stderr.truncate(cut);
            stderr_truncated = true;
        }
        Ok(RunOutput {
            exit_code: status.code(),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr,
            stdout_truncated,
            stderr_truncated,
            timed_out,
            isolated: self.mode == SandboxMode::Namespaces,
            report: report_parsed,
            duration_ms: started.elapsed().as_millis() as u64,
        })
    }

    /// Run a trivial command with reporting on, to see which isolation steps
    /// actually take effect on this host.
    pub fn probe(&self) -> Result<IsolationReport> {
        if self.mode == SandboxMode::None {
            return Err(Error::Sandbox("sandbox.mode = \"none\"：命令不隔离运行".into()));
        }
        // Unique per call: probes may run concurrently in one process (tests,
        // parallel tool calls) and must not delete each other's workspace.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("hatch-sbx-probe-{}-{seq}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let ws = dir.canonicalize()?;
        let r = self.run(&ws, &ws, &["/bin/sh".into(), "-c".into(), "id -u".into()], Duration::from_secs(10), true);
        let _ = std::fs::remove_dir_all(&dir);
        let out = r?;
        let mut report = out.report.ok_or_else(|| Error::Sandbox(format!("沙箱自检没有返回报告（exit={:?}，stderr={}）", out.exit_code, out.stderr.trim())))?;
        if let Ok(uid) = out.stdout.trim().parse() {
            report.uid = uid;
        }
        Ok(report)
    }
}
