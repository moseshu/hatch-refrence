//! Real-process test of `hatch daemon`: start → a job runs → SIGTERM →
//! graceful exit (status 0, lock released, state saved) → restart recovers
//! state (a finished once-job is not re-run) → `hatch daemon stop`.

use hatch_store::{params, Db};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_hatch");

fn hatch(home: &Path) -> Command {
    let mut c = Command::new(BIN);
    c.arg("--home").arg(home);
    for v in [
        "HATCH_LLM_BASE_URL",
        "HATCH_LLM_MODEL",
        "HATCH_LLM_API_KEY",
        "HATCH_HOME",
    ] {
        c.env_remove(v);
    }
    c
}

fn ok(home: &Path, args: &[&str]) -> String {
    let out = hatch(home).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn add_once_job(home: &Path, name: &str) {
    let at = (chrono::Utc::now() + chrono::Duration::seconds(2))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    ok(
        home,
        &[
            "jobs",
            "add",
            "--kind",
            "memory_flush",
            "--name",
            name,
            "--schedule",
            &format!("once {at}"),
            "--delivery",
            "silent",
        ],
    );
}

fn ok_runs(home: &Path, name: &str) -> i64 {
    let db = Db::open(&home.join("hatch.db")).unwrap();
    let n = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM job_runs r JOIN jobs j ON j.id = r.job_id WHERE j.name = ?1 AND r.status = 'ok'",
            params![name],
            |r| r.get(0),
        )
        .unwrap();
    n
}

fn wait_until(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("timed out waiting for {what}");
}

fn start(home: &Path) -> Child {
    hatch(home)
        .args(["daemon", "run", "--tick-secs", "1"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn wait_exit(child: &mut Child, secs: u64) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(st) = child.try_wait().unwrap() {
            return st;
        }
        assert!(Instant::now() < deadline, "daemon did not exit");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn daemon_runs_jobs_stops_gracefully_and_recovers_state() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    ok(home, &["init"]);
    add_once_job(home, "first");

    let mut d = start(home);
    wait_until("the first job to run", 20, || ok_runs(home, "first") == 1);
    assert!(home.join("daemon.pid").exists());
    assert!(ok(home, &["daemon", "status"]).contains("运行中"));
    // A second daemon is refused while the first holds the lock.
    let second = hatch(home)
        .args(["daemon", "run", "--once"])
        .output()
        .unwrap();
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("已在运行"));

    // SIGTERM → graceful exit with status 0, lock released, state saved.
    unsafe {
        libc::kill(d.id() as i32, libc::SIGTERM);
    }
    let st = wait_exit(&mut d, 15);
    assert!(st.success(), "exit status {st:?}");
    assert!(!home.join("daemon.pid").exists(), "lock released");
    let db = Db::open(&home.join("hatch.db")).unwrap();
    assert_eq!(db.meta("daemon_state").unwrap().as_deref(), Some("stopped"));
    drop(db);

    // Restart: the finished once-job is not re-run; a new one runs.
    add_once_job(home, "second");
    let mut d = start(home);
    wait_until("the second job to run", 20, || ok_runs(home, "second") == 1);
    assert_eq!(ok_runs(home, "first"), 1, "state survived the restart");
    // `hatch daemon stop` (stop file + SIGTERM) also ends it cleanly.
    let out = ok(home, &["daemon", "stop", "--wait-secs", "15"]);
    assert!(out.contains("已停止"), "{out}");
    assert!(wait_exit(&mut d, 15).success());
    assert!(ok(home, &["daemon", "status"]).contains("未运行"));
}
