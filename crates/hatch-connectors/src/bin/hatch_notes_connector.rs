//! `hatch-notes-connector` — reference connector (local file notes).
//!
//! A subprocess CLI speaking the runner protocol (one JSON request line on
//! stdin, one JSON response line on stdout). Notes live as `<name>.md`
//! files under the directory named by `HATCH_NOTES_DIR`.
//!
//! Ops: `status` (read), `list_notes` (read), `read_note` (read),
//! `write_note` (side-effect), `delete_note` (side-effect). Write ops are
//! side-effecting at the framework level too: the tool layer routes them
//! through the approval queue; this binary only executes what the runner
//! asks it to. Error messages never include note contents.

use hatch_connectors::framework::protocol::{
    response_line, ConnectorError, ConnectorRequest, ConnectorResponse, ERR_INTERNAL,
    ERR_INVALID_ARGS, ERR_NOT_CONFIGURED, ERR_NOT_FOUND, ERR_UNKNOWN_OP,
};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

fn err(code: &str, message: impl Into<String>) -> ConnectorResponse {
    ConnectorResponse {
        ok: false,
        result: None,
        error: Some(ConnectorError::new(code, message)),
    }
}

fn ok(result: Value) -> ConnectorResponse {
    ConnectorResponse { ok: true, result: Some(result), error: None }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn arg_str<'v>(args: &'v Value, key: &str) -> Result<&'v str, ConnectorResponse> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| err(ERR_INVALID_ARGS, format!("missing string argument `{key}`")))
}

fn notes_dir() -> Result<PathBuf, ConnectorResponse> {
    match std::env::var("HATCH_NOTES_DIR") {
        Ok(d) if !d.trim().is_empty() => Ok(PathBuf::from(d)),
        _ => Err(err(
            ERR_NOT_CONFIGURED,
            "HATCH_NOTES_DIR is not set — set it to a notes directory",
        )),
    }
}

fn note_path(dir: &Path, name: &str) -> Result<PathBuf, ConnectorResponse> {
    if !valid_name(name) {
        return Err(err(
            ERR_INVALID_ARGS,
            "note name must be 1-64 chars of [A-Za-z0-9_-]",
        ));
    }
    Ok(dir.join(format!("{name}.md")))
}

fn handle(req: ConnectorRequest) -> ConnectorResponse {
    let dir = match notes_dir() {
        Ok(d) => d,
        Err(e) => return e,
    };
    match req.op.as_str() {
        "status" => ok(json!({
            "status": "ok",
            "message": format!("notes directory {}", dir.display()),
        })),
        "list_notes" => {
            let mut names = Vec::new();
            match std::fs::read_dir(&dir) {
                Ok(entries) => {
                    for e in entries.flatten() {
                        let p = e.path();
                        if p.extension().and_then(|x| x.to_str()) == Some("md") {
                            if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                                names.push(stem.to_string());
                            }
                        }
                    }
                    names.sort();
                    ok(json!({ "notes": names }))
                }
                Err(e) => err(ERR_INTERNAL, format!("cannot list notes directory: {e}")),
            }
        }
        "read_note" => {
            let name = match arg_str(&req.args, "name") {
                Ok(n) => n,
                Err(e) => return e,
            };
            let path = match note_path(&dir, name) {
                Ok(p) => p,
                Err(e) => return e,
            };
            match std::fs::read_to_string(&path) {
                Ok(content) => ok(json!({ "name": name, "content": content })),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    err(ERR_NOT_FOUND, format!("no note named `{name}`"))
                }
                Err(e) => err(ERR_INTERNAL, format!("cannot read note: {e}")),
            }
        }
        "write_note" => {
            let name = match arg_str(&req.args, "name") {
                Ok(n) => n,
                Err(e) => return e,
            };
            let content = match arg_str(&req.args, "content") {
                Ok(c) => c,
                Err(e) => return e,
            };
            let path = match note_path(&dir, name) {
                Ok(p) => p,
                Err(e) => return e,
            };
            if let Err(e) = std::fs::create_dir_all(&dir) {
                return err(ERR_INTERNAL, format!("cannot create notes directory: {e}"));
            }
            match std::fs::write(&path, content) {
                Ok(()) => ok(json!({ "name": name, "written": true })),
                Err(e) => err(ERR_INTERNAL, format!("cannot write note: {e}")),
            }
        }
        "delete_note" => {
            let name = match arg_str(&req.args, "name") {
                Ok(n) => n,
                Err(e) => return e,
            };
            let path = match note_path(&dir, name) {
                Ok(p) => p,
                Err(e) => return e,
            };
            match std::fs::remove_file(&path) {
                Ok(()) => ok(json!({ "name": name, "deleted": true })),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    err(ERR_NOT_FOUND, format!("no note named `{name}`"))
                }
                Err(e) => err(ERR_INTERNAL, format!("cannot delete note: {e}")),
            }
        }
        other => err(ERR_UNKNOWN_OP, format!("unknown op `{other}`")),
    }
}

fn main() {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    let mut lines = BufReader::new(stdin.lock()).lines();
    let Some(Ok(line)) = lines.next() else {
        let _ = writeln!(
            out,
            "{}",
            response_line(&err(ERR_INVALID_ARGS, "no request line on stdin"))
        );
        std::process::exit(2);
    };
    let resp = match serde_json::from_str::<ConnectorRequest>(&line) {
        Ok(req) => handle(req),
        Err(_) => err(ERR_INVALID_ARGS, "request line is not valid connector JSON"),
    };
    let _ = writeln!(out, "{}", response_line(&resp));
    let _ = out.flush();
}
