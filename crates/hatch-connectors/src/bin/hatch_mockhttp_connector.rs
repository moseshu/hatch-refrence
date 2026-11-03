//! `hatch-mockhttp-connector` — reference connector (mock HTTP service).
//!
//! A subprocess CLI speaking the runner protocol. It talks HTTP (via ureq)
//! to the base URL in `HATCH_MOCKHTTP_BASE_URL` — a **local mock server
//! started by tests only**, never a real provider. When the vault helper
//! injects a token it arrives as the `CONNECTOR_TOKEN` env var and is sent
//! as `Authorization: Bearer <token>`; the token value is never logged and
//! never appears in error messages.
//!
//! Ops: `status` (read, `GET /status`), `get_item` (read,
//! `GET /items/{id}`), `create_item` (side-effect, `POST /items`).

use hatch_connectors::framework::protocol::{
    response_line, ConnectorError, ConnectorRequest, ConnectorResponse, ERR_AUTH,
    ERR_INTERNAL, ERR_INVALID_ARGS, ERR_NOT_CONFIGURED, ERR_NOT_FOUND, ERR_UNKNOWN_OP,
    ERR_UPSTREAM,
};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::time::Duration;

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

fn arg_str<'v>(args: &'v Value, key: &str) -> Result<&'v str, ConnectorResponse> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| err(ERR_INVALID_ARGS, format!("missing string argument `{key}`")))
}

/// GET/POST against the mock service. Upstream failures are mapped to a
/// stable code; the raw upstream body is never put in the error.
fn call_upstream(
    method: &str,
    url: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> Result<Value, ConnectorResponse> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .build();
    let mut req = match method {
        "GET" => agent.get(url),
        "POST" => agent.post(url),
        _ => return Err(err(ERR_INTERNAL, "unsupported method")),
    };
    if let Some(t) = token {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    let resp = match body {
        Some(b) => req.send_json(b),
        None => req.call(),
    };
    match resp {
        Ok(r) => r.into_json::<Value>().map_err(|_| {
            err(ERR_UPSTREAM, "mock service returned non-JSON")
        }),
        Err(ureq::Error::Status(401, _)) => {
            Err(err(ERR_AUTH, "mock service rejected the credentials"))
        }
        Err(ureq::Error::Status(404, _)) => Err(err(ERR_NOT_FOUND, "item not found")),
        Err(ureq::Error::Status(code, _)) => {
            Err(err(ERR_UPSTREAM, format!("mock service returned HTTP {code}")))
        }
        Err(ureq::Error::Transport(_)) => {
            Err(err(ERR_UPSTREAM, "could not reach the mock service"))
        }
    }
}

fn handle(req: ConnectorRequest) -> ConnectorResponse {
    let base = match std::env::var("HATCH_MOCKHTTP_BASE_URL") {
        Ok(b) if !b.trim().is_empty() => b.trim_end_matches('/').to_string(),
        _ => {
            return err(
                ERR_NOT_CONFIGURED,
                "HATCH_MOCKHTTP_BASE_URL is not set — start the local mock server first",
            )
        }
    };
    let token = std::env::var("CONNECTOR_TOKEN").ok().filter(|t| !t.is_empty());
    match req.op.as_str() {
        "status" => match call_upstream("GET", &format!("{base}/status"), token.as_deref(), None)
        {
            Ok(v) => ok(json!({
                "status": "ok",
                "message": v.get("message").and_then(Value::as_str).unwrap_or("mock service ok"),
            })),
            Err(e) => e,
        },
        "get_item" => {
            let id = match arg_str(&req.args, "id") {
                Ok(i) => i,
                Err(e) => return e,
            };
            match call_upstream(
                "GET",
                &format!("{base}/items/{id}"),
                token.as_deref(),
                None,
            ) {
                Ok(v) => ok(v),
                Err(e) => e,
            }
        }
        "create_item" => {
            let name = match arg_str(&req.args, "name") {
                Ok(n) => n,
                Err(e) => return e,
            };
            match call_upstream(
                "POST",
                &format!("{base}/items"),
                token.as_deref(),
                Some(json!({ "name": name })),
            ) {
                Ok(v) => ok(v),
                Err(e) => e,
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
