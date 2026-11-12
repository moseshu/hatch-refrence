//! Real-process test of `hatch task run --background` (M1): the
//! parent returns immediately, the detached worker runs the brief
//! task and blocks on its artifact approval, and `task show` reads
//! the same database the worker wrote. Scripted model, no sends.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_hatch");

/// `hatch --home <home>`, with any live-model environment scrubbed so
/// the background worker can never pick up a real model by accident.
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

#[test]
fn background_run_returns_immediately_and_blocks_on_approval() {
    let home = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let world: PathBuf =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../eval/worlds/meeting-brief");
    // The parent must return promptly — it spawns the worker, it does
    // not wait for it.
    let start = Instant::now();
    let output = hatch(home.path())
        .args(["task", "run", "--world"])
        .arg(&world)
        .args(["--out"])
        .arg(out.path())
        .arg("--background")
        .output()
        .unwrap();
    assert!(output.status.success(), "后台启动应成功");
    assert!(
        start.elapsed() < Duration::from_secs(30),
        "后台模式应立即返回，不等 worker 完成"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("后台 worker 已启动"),
        "应返回后台启动信息，实际 {stdout}"
    );
    assert!(
        stdout.contains("worker.log"),
        "应给出日志路径，实际 {stdout}"
    );
    // Poll the worker's log until it blocks on the artifact approval.
    let log = out.path().join("worker.log");
    let mut text = String::new();
    for _ in 0..150 {
        if let Ok(t) = std::fs::read_to_string(&log) {
            text = t;
            if text.contains("已暂停") {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        text.contains("已暂停"),
        "后台 worker 应在审批上暂停，日志：{text}"
    );
    // The task id is in the log; `task show` reads the very same DB
    // the background worker wrote.
    let id = text
        .split_whitespace()
        .find(|w| w.starts_with("tsk_"))
        .expect("日志应含任务 id");
    let show = hatch(home.path())
        .args(["task", "show"])
        .arg(id)
        .output()
        .unwrap();
    let show_text = String::from_utf8_lossy(&show.stdout);
    assert!(
        show_text.contains("blocked") || show_text.contains("阻塞"),
        "任务应为阻塞态，实际 {show_text}"
    );
    assert!(!out.path().join("brief-v1.md").exists(), "批准前不得写文件");
}
