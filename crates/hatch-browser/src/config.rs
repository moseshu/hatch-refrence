//! Browser configuration, profile names and Chromium discovery.

use crate::error::{BResult, BrowserError};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Profiles are isolated browsers: cookies, logins and storage of one are
/// invisible to the others.
pub const PROFILE_MAIN: &str = "main";
pub const PROFILE_SCHEDULER: &str = "scheduler";
pub const PROFILE_RESEARCH: &str = "research";
pub const PROFILES: &[&str] = &[PROFILE_MAIN, PROFILE_SCHEDULER, PROFILE_RESEARCH];

/// Which profile an action from `origin` uses: sub-agents browse in the
/// research profile, unattended runs in the scheduler profile, everything
/// else (the conversation) in the main profile.
pub fn profile_for_origin(origin: &str) -> &'static str {
    if origin.starts_with("subagent:") {
        PROFILE_RESEARCH
    } else if origin.starts_with("triage:") || origin.starts_with("job:") || origin.starts_with("worker:") {
        PROFILE_SCHEDULER
    } else {
        PROFILE_MAIN
    }
}

pub fn validate_profile(name: &str) -> BResult<()> {
    let ok = !name.is_empty() && name.len() <= 32 && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(BrowserError::PolicyDenied(format!("无效的浏览器 profile 名称：{name}")))
    }
}

#[derive(Clone, Debug)]
pub struct BrowserConfig {
    /// Chromium / Chrome executable; `None` = search the usual places.
    pub chromium_path: Option<PathBuf>,
    pub headless: bool,
    /// Parent of the per-profile `user-data-dir`s.
    pub profiles_dir: PathBuf,
    /// Where screenshots are written.
    pub screenshots_dir: PathBuf,
    /// Parent of the per-session download directories.
    pub downloads_dir: PathBuf,
    /// Open tabs per profile.
    pub max_tabs: usize,
    /// Tabs unused this long are closed; a browser without tabs is stopped.
    pub idle_timeout: Duration,
    /// Allow loopback destinations (127.0.0.1, ::1). Test setups only.
    pub allow_loopback: bool,
    /// Extra host patterns that are always refused (`example.com`, `*.example.com`).
    pub blocklist: Vec<String>,
    /// Ignore TLS certificate errors. Test setups only — never in production.
    pub ignore_cert_errors: bool,
    /// Extra CA bundle (PEM) imported into each profile's NSS store.
    pub ca_bundle: Option<PathBuf>,
    pub nav_timeout: Duration,
}

impl BrowserConfig {
    pub fn new(base_dir: &Path) -> Self {
        BrowserConfig {
            chromium_path: None,
            headless: true,
            profiles_dir: base_dir.join("browser").join("profiles"),
            screenshots_dir: base_dir.join("browser").join("screenshots"),
            downloads_dir: base_dir.join("browser").join("downloads"),
            max_tabs: 4,
            idle_timeout: Duration::from_secs(600),
            allow_loopback: false,
            blocklist: Vec::new(),
            ignore_cert_errors: false,
            ca_bundle: std::env::var_os("HATCH_CA_BUNDLE").or_else(|| std::env::var_os("SSL_CERT_FILE")).map(PathBuf::from),
            nav_timeout: Duration::from_secs(30),
        }
    }

    pub fn profile_dir(&self, profile: &str) -> PathBuf {
        self.profiles_dir.join(profile)
    }
}

/// Well-known Chromium locations, tried in order after an explicit path.
const CANDIDATES: &[&str] = &[
    "/opt/pw-browsers/chromium",
    "/usr/bin/chromium",
    "/usr/bin/chromium-browser",
    "/usr/bin/google-chrome",
    "/usr/bin/google-chrome-stable",
    "/snap/bin/chromium",
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
];

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

/// Find a Chromium binary: explicit path, `HATCH_CHROMIUM`, well-known
/// locations, then `PATH`.
pub fn find_chromium(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return is_executable(p).then(|| p.to_path_buf());
    }
    if let Some(p) = std::env::var_os("HATCH_CHROMIUM").map(PathBuf::from) {
        if is_executable(&p) {
            return Some(p);
        }
    }
    for c in CANDIDATES {
        let p = Path::new(c);
        if is_executable(p) {
            return Some(p.to_path_buf());
        }
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for name in ["chromium", "chromium-browser", "google-chrome", "google-chrome-stable"] {
            let p = dir.join(name);
            if is_executable(&p) {
                return Some(p);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_by_origin() {
        assert_eq!(profile_for_origin("chat:s1"), PROFILE_MAIN);
        assert_eq!(profile_for_origin("subagent:sa_1"), PROFILE_RESEARCH);
        assert_eq!(profile_for_origin("triage:run1"), PROFILE_SCHEDULER);
        assert!(validate_profile("main").is_ok());
        assert!(validate_profile("../etc").is_err());
        assert!(validate_profile("").is_err());
        let c = BrowserConfig::new(Path::new("/tmp/h"));
        assert_eq!(c.profile_dir("main"), Path::new("/tmp/h/browser/profiles/main"));
        assert!(!c.allow_loopback && !c.ignore_cert_errors && c.headless);
    }

    #[test]
    fn explicit_path_must_be_executable() {
        assert!(find_chromium(Some(Path::new("/definitely/not/here"))).is_none());
    }
}
