//! The daemon loop: tick, sleep, repeat — and stop cleanly.
//!
//! Stop requests come from three places, all turned into cancellation of the
//! daemon's root token by a watcher thread (polled every 200 ms):
//! a stop file (`hatch daemon stop`), SIGTERM/SIGINT (when `use_signals`) or
//! the caller's own token. The running job stops at its next cancellation
//! point; its firing stays pending and is re-run on the next start. State is
//! saved, the pid lock released, and the loop returns normally.

use crate::clock::Clock;
use crate::engine::Engine;
use crate::lock::{pid_alive, read_pid, PidLock};
use crate::store::JobStore;
use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::{CancelToken, Result};
use hatch_store::Db;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const POLL: Duration = Duration::from_millis(200);

#[derive(Clone, Debug)]
pub struct DaemonPaths {
    pub lock: PathBuf,
    pub stop: PathBuf,
}

impl DaemonPaths {
    pub fn in_dir(home: &Path) -> DaemonPaths {
        DaemonPaths { lock: home.join("daemon.pid"), stop: home.join("daemon.stop") }
    }
}

#[derive(Clone, Debug)]
pub struct DaemonConfig {
    pub tick: Duration,
    /// Stop after this many ticks (tests, `--once`).
    pub max_ticks: Option<u64>,
    /// Treat SIGTERM / SIGINT as stop requests (installs the handlers).
    pub use_signals: bool,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        DaemonConfig { tick: Duration::from_secs(60), max_ticks: None, use_signals: false }
    }
}

#[derive(Clone, Debug, Default)]
pub struct DaemonExit {
    pub ticks: u64,
    pub runs: usize,
    pub reason: String,
}

#[derive(Clone, Debug)]
pub struct DaemonStatus {
    pub pid: Option<i32>,
    pub alive: bool,
    pub state: String,
    pub started_at: String,
    pub last_tick: String,
    pub stopped_at: String,
}

pub struct Daemon<'a> {
    pub engine: &'a Engine<'a>,
    pub db: &'a Db,
    pub paths: DaemonPaths,
    pub config: DaemonConfig,
    pub clock: &'a dyn Clock,
}

fn set(db: &Db, key: &str, value: &str) {
    let _ = db.set_meta(key, value);
}

impl Daemon<'_> {
    pub fn run(&self, cancel: &CancelToken) -> Result<DaemonExit> {
        let _lock = PidLock::acquire(&self.paths.lock)?;
        // A stop file from an earlier `daemon stop` must not stop this run.
        let _ = std::fs::remove_file(&self.paths.stop);
        if self.config.use_signals {
            crate::signals::install();
        }
        let root = cancel.child();
        let finished = Arc::new(AtomicBool::new(false));
        let watcher = {
            let (root, finished, stop) = (root.clone(), finished.clone(), self.paths.stop.clone());
            let signals = self.config.use_signals;
            std::thread::spawn(move || {
                while !finished.load(Ordering::SeqCst) && !root.is_cancelled() {
                    if stop.exists() {
                        root.cancel("stop file");
                    } else if signals && crate::signals::stop_requested() {
                        root.cancel("signal");
                    }
                    std::thread::sleep(POLL);
                }
            })
        };
        let now = self.clock.now();
        let reaped = JobStore::new(self.db).reap_interrupted(&now)?;
        set(self.db, "daemon_state", "running");
        set(self.db, "daemon_started_at", &to_rfc3339(&now));
        self.db.audit("daemon", "started", &format!("pid {} tick {:?}; {reaped} interrupted run(s) marked failed", std::process::id(), self.config.tick))?;
        let mut exit = DaemonExit::default();
        let result = self.lp(&root, &mut exit);
        finished.store(true, Ordering::SeqCst);
        let _ = watcher.join();
        exit.reason = match (&result, root.reason()) {
            (Err(e), _) => format!("error: {e}"),
            (Ok(()), Some(r)) => r,
            (Ok(()), None) => "max ticks".into(),
        };
        let now = self.clock.now();
        set(self.db, "daemon_state", "stopped");
        set(self.db, "daemon_stopped_at", &to_rfc3339(&now));
        self.db.audit("daemon", "stopped", &format!("{} after {} tick(s), {} run(s)", exit.reason, exit.ticks, exit.runs))?;
        let _ = std::fs::remove_file(&self.paths.stop);
        result.map(|_| exit)
    }

    fn lp(&self, root: &CancelToken, exit: &mut DaemonExit) -> Result<()> {
        loop {
            if root.is_cancelled() {
                return Ok(());
            }
            let now: Timestamp = self.clock.now();
            match self.engine.tick(&now, root) {
                Ok(r) => exit.runs += r.runs.len(),
                Err(e) => self.db.audit("daemon", "tick_error", &e.to_string())?,
            }
            set(self.db, "daemon_last_tick", &to_rfc3339(&now));
            exit.ticks += 1;
            if self.config.max_ticks.is_some_and(|m| exit.ticks >= m) {
                return Ok(());
            }
            if root.sleep(self.config.tick) {
                return Ok(());
            }
        }
    }
}

/// What the pid file and the database say about the daemon.
pub fn status(db: &Db, paths: &DaemonPaths) -> Result<DaemonStatus> {
    let pid = read_pid(&paths.lock);
    let meta = |k: &str| db.meta(k).map(|v| v.unwrap_or_default());
    Ok(DaemonStatus {
        pid,
        alive: pid.is_some_and(pid_alive),
        state: meta("daemon_state")?,
        started_at: meta("daemon_started_at")?,
        last_tick: meta("daemon_last_tick")?,
        stopped_at: meta("daemon_stopped_at")?,
    })
}

/// Ask a running daemon to stop: stop file plus SIGTERM. Returns the pid signalled.
pub fn request_stop(paths: &DaemonPaths) -> Result<Option<i32>> {
    std::fs::write(&paths.stop, b"stop\n")?;
    let pid = read_pid(&paths.lock).filter(|p| pid_alive(*p));
    if let Some(p) = pid {
        // SAFETY: plain kill(2) on a pid read from our own lock file.
        unsafe {
            libc::kill(p, libc::SIGTERM);
        }
    }
    Ok(pid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::SystemClock;
    use crate::engine::{FnHandler, JobContext, JobOutcome};
    use crate::job::NewJob;
    use std::sync::atomic::AtomicU32;

    #[test]
    fn stop_file_ends_the_loop_cleanly_and_releases_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_in_memory().unwrap();
        let tz = chrono_tz::Asia::Shanghai;
        let n = Arc::new(AtomicU32::new(0));
        let n2 = n.clone();
        let engine = Engine::new(&db, tz).register(
            "k",
            Arc::new(FnHandler(move |_: &JobContext| {
                n2.fetch_add(1, Ordering::SeqCst);
                Ok(JobOutcome::Done { summary: "ok".into() })
            })),
        );
        let past = hatch_core::time::now() - chrono::Duration::minutes(31);
        JobStore::new(&db).add(&NewJob::new("j", "k", "every 30m"), &past, &tz).unwrap();
        let paths = DaemonPaths::in_dir(dir.path());
        let d = Daemon { engine: &engine, db: &db, paths: paths.clone(), config: DaemonConfig { tick: Duration::from_secs(30), ..Default::default() }, clock: &SystemClock };
        let stop = paths.stop.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            std::fs::write(stop, "stop").unwrap();
        });
        let started = std::time::Instant::now();
        let exit = d.run(&CancelToken::new()).unwrap();
        t.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(5), "segmented sleep reacts quickly");
        assert_eq!((exit.ticks, exit.runs, exit.reason.as_str()), (1, 1, "stop file"));
        assert_eq!(n.load(Ordering::SeqCst), 1);
        assert!(!paths.lock.exists() && !paths.stop.exists());
        let st = status(&db, &paths).unwrap();
        assert_eq!((st.state.as_str(), st.alive), ("stopped", false));
    }

    #[test]
    fn a_second_daemon_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_in_memory().unwrap();
        let engine = Engine::new(&db, chrono_tz::UTC);
        let paths = DaemonPaths::in_dir(dir.path());
        std::fs::write(&paths.lock, "1\n").unwrap(); // pid 1 is alive
        let d = Daemon { engine: &engine, db: &db, paths, config: DaemonConfig { max_ticks: Some(1), ..Default::default() }, clock: &SystemClock };
        assert!(d.run(&CancelToken::new()).is_err());
    }
}
