//! REPL `/image <path>` queues an image for the next message: a valid
//! file is accepted with a count, a missing file and a bare `/image`
//! are refused without ending the session.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn hatch_bin() -> PathBuf {
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_hatch") {
        return p.into();
    }
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let plain = manifest.join("../target/debug/hatch");
    if plain.is_file() {
        return plain;
    }
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let ok = Command::new(&cargo)
        .args(["build", "-p", "hatch-cli", "--bin", "hatch"])
        .current_dir(manifest.parent().unwrap())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok && plain.is_file(), "could not build hatch binary");
    plain
}

#[test]
fn repl_image_command_queues_and_refuses() {
    let home = tempfile::tempdir().unwrap();
    let init = Command::new(hatch_bin())
        .arg("--home")
        .arg(home.path())
        .arg("init")
        .output()
        .unwrap();
    assert!(init.status.success(), "init: {}", String::from_utf8_lossy(&init.stderr));
    // A real 1x1 PNG (magic bytes are what the sniff checks).
    let png = home.path().join("dot.png");
    std::fs::write(
        &png,
        [
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52,
        ],
    )
    .unwrap();
    let mut child = Command::new(hatch_bin())
        .arg("--home")
        .arg(home.path())
        .args(["chat", "--new"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let input = format!(
        "/image {}\n/image /nonexistent.png\n/image\n/exit\n",
        png.display()
    );
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "chat exited badly");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains("已附带图片（共 1 张）"),
        "stdout: {stdout}"
    );
    assert!(stderr.contains("附带图片失败"), "stderr: {stderr}");
    assert!(stderr.contains("用法：/image"), "stderr: {stderr}");
}
