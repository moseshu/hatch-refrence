//! Starting Chromium for a profile, and finding one that is already running.
//!
//! * Processes are spawned from one long-lived launcher thread: Linux sends
//!   `PR_SET_PDEATHSIG` when the *thread* that forked the child exits, so
//!   spawning from short-lived worker threads (sub-agents) would kill the
//!   browser early. The browser dies with the hatch process, not a thread.
//! * Each browser runs in its own process group so the whole tree can be
//!   killed on shutdown.
//! * The DevTools endpoint is written to `<profile>/hatch-devtools.json`; any
//!   other hatch process (for example `hatch approvals approve`) re-attaches
//!   to a running browser through it instead of starting a second one.
//! * `HOME` points into the profile, so each profile has its own NSS store;
//!   an extra CA bundle (corporate proxy) is imported there with `certutil`.

use crate::config::{find_chromium, validate_profile, BrowserConfig};
use crate::error::{BResult, BrowserError};
use crate::http::local_json;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const ENDPOINT_FILE: &str = "hatch-devtools.json";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Endpoint {
    pub pid: u32,
    pub port: u16,
    /// The hatch process that started (and owns) the browser.
    pub owner_pid: u32,
    pub headless: bool,
}

type SpawnReq = (Command, mpsc::Sender<std::io::Result<Child>>);

fn launcher() -> &'static Mutex<mpsc::Sender<SpawnReq>> {
    static L: OnceLock<Mutex<mpsc::Sender<SpawnReq>>> = OnceLock::new();
    L.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<SpawnReq>();
        std::thread::Builder::new()
            .name("hatch-browser-launcher".into())
            .spawn(move || {
                for (mut cmd, reply) in rx {
                    let _ = reply.send(cmd.spawn());
                }
            })
            .expect("spawn launcher thread");
        Mutex::new(tx)
    })
}

fn spawn_on_launcher(cmd: Command) -> std::io::Result<Child> {
    let (tx, rx) = mpsc::channel();
    launcher().lock().unwrap_or_else(|e| e.into_inner()).send((cmd, tx)).map_err(|_| std::io::Error::other("launcher thread gone"))?;
    rx.recv().map_err(|_| std::io::Error::other("launcher thread gone"))?
}

pub fn pid_alive(pid: u32) -> bool {
    // SAFETY: kill with signal 0 only checks for existence.
    pid > 0 && unsafe { libc::kill(pid as libc::pid_t, 0) } == 0
}

/// A running browser for `profile` with a responding DevTools port.
pub fn running_endpoint(cfg: &BrowserConfig, profile: &str) -> Option<Endpoint> {
    let path = cfg.profile_dir(profile).join(ENDPOINT_FILE);
    let ep: Endpoint = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    if !pid_alive(ep.pid) {
        return None;
    }
    local_json(ep.port, "GET", "/json/version", Duration::from_secs(2)).ok()?;
    Some(ep)
}

fn proxy_args() -> Vec<String> {
    let get = |names: &[&str]| names.iter().filter_map(|n| std::env::var(n).ok()).map(|v| v.trim().to_string()).find(|v| !v.is_empty());
    let mut args = Vec::new();
    if let Some(p) = get(&["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy", "HTTP_PROXY", "http_proxy"]) {
        args.push(format!("--proxy-server={p}"));
        if let Some(np) = get(&["NO_PROXY", "no_proxy"]) {
            let list: Vec<&str> = np.split(',').map(str::trim).filter(|e| !e.is_empty() && !e.contains('/')).collect();
            if !list.is_empty() {
                args.push(format!("--proxy-bypass-list={}", list.join(";")));
            }
        }
    }
    args
}

/// Import the PEM certificates of `bundle` into `<home>/.pki/nssdb` once.
/// Returns a note when it could not be done (the browser still starts).
pub fn ensure_ca(home: &Path, bundle: &Path) -> Option<String> {
    let pem = match std::fs::read_to_string(bundle) {
        Ok(p) => p,
        Err(e) => return Some(format!("无法读取 CA 文件 {}：{e}", bundle.display())),
    };
    let certutil = ["/usr/bin/certutil", "/usr/local/bin/certutil"].into_iter().map(PathBuf::from).find(|p| p.exists());
    let Some(certutil) = certutil else {
        return Some("未找到 certutil（libnss3-tools），未导入额外 CA；经代理访问 https 可能失败".into());
    };
    let db = home.join(".pki").join("nssdb");
    let marker = db.join("hatch-ca.stamp");
    let stamp = hatch_core::ids::stable_id("ca", &[&pem]);
    if std::fs::read_to_string(&marker).ok().as_deref() == Some(stamp.as_str()) {
        return None;
    }
    if std::fs::create_dir_all(&db).is_err() {
        return Some("无法创建 NSS 目录".into());
    }
    let dbarg = format!("sql:{}", db.display());
    if !db.join("cert9.db").exists() {
        let _ = Command::new(&certutil).args(["-N", "-d", &dbarg, "--empty-password"]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
    let mut n = 0;
    for (i, block) in pem.split("-----END CERTIFICATE-----").enumerate() {
        let Some(start) = block.find("-----BEGIN CERTIFICATE-----") else { continue };
        let cert = format!("{}-----END CERTIFICATE-----\n", &block[start..]);
        let tmp = db.join("import.pem");
        if std::fs::write(&tmp, cert).is_err() {
            continue;
        }
        let ok = Command::new(&certutil)
            .args(["-A", "-d", &dbarg, "-n", &format!("hatch-ca-{i}"), "-t", "C,,", "-i"])
            .arg(&tmp)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if ok {
            n += 1;
        }
        let _ = std::fs::remove_file(&tmp);
    }
    let _ = std::fs::write(&marker, stamp);
    (n == 0).then(|| "CA 文件中没有导入任何证书".into())
}

/// A browser this process started.
pub struct Launched {
    pub child: Child,
    pub endpoint: Endpoint,
    /// Problems worth showing in `hatch doctor` (CA import etc.).
    pub notes: Vec<String>,
}

pub fn chromium_args(cfg: &BrowserConfig, profile_dir: &Path, headless: bool, start_url: &str) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "--remote-debugging-port=0".into(),
        "--remote-debugging-address=127.0.0.1".into(),
        format!("--user-data-dir={}", profile_dir.join("data").display()),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--disable-background-networking".into(),
        "--disable-sync".into(),
        "--disable-extensions".into(),
        "--disable-component-update".into(),
        "--disable-default-apps".into(),
        "--disable-features=Translate,MediaRouter,OptimizationHints,AutofillServerCommunication".into(),
        "--metrics-recording-only".into(),
        "--mute-audio".into(),
        "--disable-dev-shm-usage".into(),
        "--password-store=basic".into(),
        "--window-size=1280,900".into(),
    ];
    if headless {
        a.push("--headless".into());
        a.push("--disable-gpu".into());
    }
    // Chromium refuses to start its own sandbox as root (containers).
    // SAFETY: geteuid has no preconditions.
    // CI/test-only escape hatch: some hosts (e.g. GitHub Actions runners)
    // cannot start Chromium's zygote/sandbox even as a non-root user.
    // Set HATCH_BROWSER_NO_SANDBOX=1 only in such test environments;
    // production browsing keeps Chromium's own sandbox.
    if unsafe { libc::geteuid() } == 0
        || std::env::var("HATCH_BROWSER_NO_SANDBOX")
            .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
    {
        a.push("--no-sandbox".into());
    }
    if cfg.ignore_cert_errors {
        a.push("--ignore-certificate-errors".into());
    }
    a.extend(proxy_args());
    a.push(start_url.into());
    a
}

/// Start Chromium for `profile`. Fails if a browser already uses the profile.
pub fn launch(cfg: &BrowserConfig, profile: &str, headless: bool, start_url: &str) -> BResult<Launched> {
    validate_profile(profile)?;
    let exe = find_chromium(cfg.chromium_path.as_deref())
        .ok_or_else(|| BrowserError::Unavailable("找不到 Chromium/Chrome，可在 [browser] chromium_path 配置或设置 HATCH_CHROMIUM".into()))?;
    let dir = cfg.profile_dir(profile);
    let home = dir.join("home");
    std::fs::create_dir_all(&home).map_err(|e| BrowserError::Unavailable(format!("创建 profile 目录：{e}")))?;
    let mut notes = Vec::new();
    if let Some(bundle) = &cfg.ca_bundle {
        notes.extend(ensure_ca(&home, bundle));
    }
    let mut cmd = Command::new(&exe);
    cmd.args(chromium_args(cfg, &dir, headless, start_url)).env("HOME", &home).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    if headless {
        cmd.env_remove("DISPLAY").env_remove("WAYLAND_DISPLAY");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
        #[cfg(target_os = "linux")]
        // SAFETY: prctl is async-signal-safe and only affects the child.
        unsafe {
            cmd.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = spawn_on_launcher(cmd).map_err(|e| BrowserError::Unavailable(format!("启动 {} 失败：{e}", exe.display())))?;
    let stderr = child.stderr.take().ok_or_else(|| BrowserError::Unavailable("no stderr".into()))?;
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::Builder::new()
        .name("hatch-browser-stderr".into())
        .spawn(move || {
            let mut tail: Vec<String> = Vec::new();
            let mut sent = false;
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if !sent && line.contains("DevTools listening on ws://") {
                    sent = tx.send(line.clone()).is_ok();
                } else if !sent {
                    tail.push(line);
                    if tail.len() > 20 {
                        tail.remove(0);
                    }
                }
            }
            if !sent {
                let _ = tx.send(format!("EXIT:{}", tail.join(" | ")));
            }
        })
        .map_err(|e| BrowserError::Unavailable(e.to_string()))?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let line = loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(l) => break l,
            Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() < deadline => continue,
            _ => {
                kill_tree(&mut child);
                return Err(BrowserError::Unavailable("Chromium 30 秒内没有就绪".into()));
            }
        }
    };
    if let Some(tail) = line.strip_prefix("EXIT:") {
        kill_tree(&mut child);
        return Err(BrowserError::Unavailable(format!("Chromium 退出：{}", tail.chars().take(600).collect::<String>())));
    }
    let port = line
        .split("ws://127.0.0.1:")
        .nth(1)
        .and_then(|r| r.split('/').next())
        .and_then(|p| p.parse::<u16>().ok())
        .ok_or_else(|| BrowserError::Protocol(format!("无法解析 DevTools 地址：{line}")))?;
    let endpoint = Endpoint { pid: child.id(), port, owner_pid: std::process::id(), headless };
    let _ = std::fs::write(dir.join(ENDPOINT_FILE), serde_json::to_string(&endpoint).unwrap_or_default());
    Ok(Launched { child, endpoint, notes })
}

/// Kill the browser's whole process group and reap it.
pub fn kill_tree(child: &mut Child) {
    // SAFETY: negative pid = process group; the group id equals the child pid (process_group(0)).
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_are_isolated_and_safe_by_default() {
        let cfg = BrowserConfig::new(Path::new("/tmp/hb"));
        let a = chromium_args(&cfg, Path::new("/tmp/hb/browser/profiles/main"), true, "about:blank");
        assert!(a.iter().any(|x| x == "--user-data-dir=/tmp/hb/browser/profiles/main/data"));
        assert!(a.iter().any(|x| x == "--remote-debugging-address=127.0.0.1"));
        assert!(!a.iter().any(|x| x.contains("ignore-certificate-errors")), "production never ignores TLS errors");
        assert_eq!(a.last().unwrap(), "about:blank");
        assert!(running_endpoint(&cfg, "main").is_none());
    }
}
