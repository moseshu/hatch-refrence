//! Read-only evidence checks against the immutable approved payload.
//! A transport receipt locates a resource; it is never sufficient evidence.
//!
//! Executors without a dedicated read-back checker fall back to the
//! effect contract they declare ([`EffectContract`]): read-only actions
//! need no evidence, external effects are judged on their structured
//! success receipt (plus existence of any artifact path the receipt
//! declares), and local-artifact actions on the declared files
//! themselves. A dedicated checker always outranks the contract.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Fail,
    Ambiguous,
    NotApplicable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectState {
    ConfirmedApplied,
    ConfirmedNotApplied,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvidenceCheck {
    pub rule: String,
    pub verdict: Verdict,
    pub effect: EffectState,
    pub source: String,
    pub resource: String,
    pub expected_digest: String,
    pub observed_digest: String,
    pub reason: String,
}

impl EvidenceCheck {
    pub fn unknown(rule: &str, reason: impl Into<String>) -> Self {
        Self {
            rule: rule.into(),
            verdict: Verdict::Ambiguous,
            effect: EffectState::Unknown,
            source: String::new(),
            resource: String::new(),
            expected_digest: String::new(),
            observed_digest: String::new(),
            reason: reason.into(),
        }
    }
    /// The action has no durable effect to read back (read-only fetch,
    /// search, or a sandbox command whose output is its only record).
    pub fn not_applicable(rule: &str, reason: impl Into<String>) -> Self {
        Self {
            rule: rule.into(),
            verdict: Verdict::NotApplicable,
            effect: EffectState::Unknown,
            source: String::new(),
            resource: String::new(),
            expected_digest: String::new(),
            observed_digest: String::new(),
            reason: reason.into(),
        }
    }
    pub fn compare(
        rule: &str,
        source: &str,
        resource: &str,
        expected: &[u8],
        observed: &[u8],
    ) -> Self {
        let matches = expected == observed;
        Self {
            rule: rule.into(),
            verdict: if matches {
                Verdict::Pass
            } else {
                Verdict::Fail
            },
            effect: EffectState::ConfirmedApplied,
            source: source.into(),
            resource: resource.into(),
            expected_digest: crate::checkpoint::content_hash(expected),
            observed_digest: crate::checkpoint::content_hash(observed),
            reason: if matches {
                "独立回读与批准内容一致"
            } else {
                "资源存在，但独立回读与批准内容不符；不能重复原动作"
            }
            .into(),
        }
    }
}

/// The side-effect contract an executor declares at registration.
/// The completion gate keys its fallback evidence check on this type,
/// never on the concrete tool name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectContract {
    /// No durable effect (fetch, search, navigation).
    ReadOnly,
    /// Produces files on this machine; the payload declares them.
    LocalArtifact,
    /// Changes state outside this machine (third-party APIs, MCP
    /// servers, connectors). No generic read-back exists; the
    /// structured success receipt is the evidence tier.
    ExternalEffect,
}

/// Fallback evidence check for executors without a dedicated checker.
pub fn verify_by_contract(
    contract: EffectContract,
    kind: &str,
    payload: &Value,
    receipt: &str,
) -> EvidenceCheck {
    match contract {
        EffectContract::ReadOnly => EvidenceCheck::not_applicable(
            kind,
            "只读动作，无持久效果可独立回读",
        ),
        EffectContract::ExternalEffect => receipt_check(kind, receipt),
        EffectContract::LocalArtifact => artifact_check(kind, payload, receipt),
    }
}

/// Structured-receipt evidence: the receipt must be a non-error,
/// non-empty result, and every absolute file path it declares as
/// produced must exist and be non-empty.
pub fn receipt_check(kind: &str, receipt: &str) -> EvidenceCheck {
    let rule = "receipt-v1";
    let text = receipt.trim();
    if text.is_empty() {
        return EvidenceCheck::unknown(rule, "执行回执为空，无法确认动作是否生效");
    }
    if result_reports_error(text) {
        return fail(rule, kind, "执行回执报告错误，动作未生效");
    }
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        let mut paths = Vec::new();
        collect_declared_paths(&v, &mut paths);
        for p in &paths {
            if !file_nonempty(p) {
                return fail(
                    rule,
                    p,
                    format!("回执声明的产物不存在或为空：{p}"),
                );
            }
        }
    }
    EvidenceCheck {
        rule: rule.into(),
        verdict: Verdict::Pass,
        effect: EffectState::ConfirmedApplied,
        source: "structured-receipt".into(),
        resource: kind.into(),
        expected_digest: String::new(),
        observed_digest: crate::checkpoint::content_hash(text.as_bytes()),
        reason: "结构化成功回执确认动作已执行（外部效果无独立回读通道）".into(),
    }
}

/// Local-artifact evidence: every path declared in the payload's
/// `artifacts` list must exist and be non-empty. With no declared
/// artifacts this degrades to the receipt check.
pub fn artifact_check(kind: &str, payload: &Value, receipt: &str) -> EvidenceCheck {
    let rule = "artifact-v1";
    let root = payload.get("root").and_then(Value::as_str).unwrap_or("");
    let declared: Vec<String> = payload
        .get("artifacts")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(|p| {
                    if std::path::Path::new(p).is_absolute() || root.is_empty() {
                        p.to_string()
                    } else {
                        format!("{root}/{p}")
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    if declared.is_empty() {
        return receipt_check(kind, receipt);
    }
    for p in &declared {
        if !file_nonempty(p) {
            return fail(rule, p, format!("声明的本地产物不存在或为空：{p}"));
        }
    }
    EvidenceCheck {
        rule: rule.into(),
        verdict: Verdict::Pass,
        effect: EffectState::ConfirmedApplied,
        source: "filesystem".into(),
        resource: declared.join(","),
        expected_digest: String::new(),
        observed_digest: String::new(),
        reason: "声明的本地产物均存在且非空".into(),
    }
}

fn fail(rule: &str, resource: &str, reason: impl Into<String>) -> EvidenceCheck {
    EvidenceCheck {
        rule: rule.into(),
        verdict: Verdict::Fail,
        effect: EffectState::ConfirmedNotApplied,
        source: "structured-receipt".into(),
        resource: resource.into(),
        expected_digest: String::new(),
        observed_digest: String::new(),
        reason: reason.into(),
    }
}

fn file_nonempty(path: &str) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() > 0)
}

/// MCP-style `isError: true`, or a top-level non-null `error` field.
/// Does a model-facing tool result report an error? Covers the plain
/// `ERROR:` prefix and structured envelopes (`isError: true`, non-null
/// `error`), including envelopes behind a human prefix such as
/// `已执行（apr_…）：{…}`. Shared by the completion gate and the agent
/// loop's duplicate-call breaker so both read errors the same way.
pub fn result_reports_error(content: &str) -> bool {
    let text = content.trim();
    if text.starts_with("ERROR") {
        return true;
    }
    let candidates = [Some(text), text.find('{').map(|i| &text[i..])];
    for cand in candidates.into_iter().flatten() {
        if let Ok(v) = serde_json::from_str::<Value>(cand) {
            if json_reports_error(&v) {
                return true;
            }
        }
    }
    text.contains("\"isError\":true") || text.contains("\"isError\": true")
}

fn json_reports_error(v: &Value) -> bool {
    match v {
        Value::Object(map) => {
            if map.get("isError").and_then(Value::as_bool) == Some(true) {
                return true;
            }
            if let Some(e) = map.get("error") {
                if !e.is_null() && e.as_str().is_none_or(|s| !s.is_empty()) {
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

/// Absolute paths a JSON receipt declares as produced files, under
/// `path` / `file` / `output_path` keys. String values that are
/// themselves JSON objects (MCP text content) are descended into.
fn collect_declared_paths(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(map) => {
            for (k, val) in map {
                if matches!(k.as_str(), "path" | "file" | "output_path") {
                    if let Some(s) = val.as_str() {
                        if std::path::Path::new(s).is_absolute() {
                            out.push(s.to_string());
                        }
                    }
                }
                collect_declared_paths(val, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_declared_paths(item, out);
            }
        }
        Value::String(s) => {
            if let Ok(inner) = serde_json::from_str::<Value>(s) {
                if inner.is_object() || inner.is_array() {
                    collect_declared_paths(&inner, out);
                }
            }
        }
        _ => {}
    }
}

/// Whether an unapproved direct tool call needs an independent completion check.
pub fn requires_evidence(class: hatch_policy::ActionClass) -> bool {
    use hatch_policy::ActionClass::*;
    !matches!(
        class,
        ReadPersonal | FetchWeb | BrowseLocal | DraftMessage | WriteMemory
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn result_reports_error_covers_envelopes_and_prefixes() {
        assert!(result_reports_error("ERROR: boom"));
        assert!(result_reports_error(r#"{"content":[],"isError":true}"#));
        assert!(result_reports_error(r#"已执行（apr_x）：{"content":[{"text":"Error: nope","type":"text"}],"isError":true}"#));
        assert!(result_reports_error(r#"{"error":"bad"}"#));
        assert!(!result_reports_error(r#"{"content":[],"isError":false}"#));
        assert!(!result_reports_error(r#"已执行（apr_x）：{"path":"/tmp/a.png"}"#));
        assert!(!result_reports_error("plain success text"));
        assert!(!result_reports_error(""));
    }

    #[test]
    fn receipt_tiers() {
        assert_eq!(receipt_check("mcp_call", "").verdict, Verdict::Ambiguous);
        assert_eq!(receipt_check("mcp_call", "ERROR: boom").verdict, Verdict::Fail);
        assert_eq!(
            receipt_check("mcp_call", r#"{"isError":true,"content":[]}"#).verdict,
            Verdict::Fail
        );
        assert_eq!(
            receipt_check("mcp_call", r#"{"error":"nope"}"#).verdict,
            Verdict::Fail
        );
        assert_eq!(
            receipt_check("mcp_call", r#"{"isError":false,"content":[{"type":"text","text":"ok"}]}"#).verdict,
            Verdict::Pass
        );
        assert_eq!(receipt_check("connector_call", "done").verdict, Verdict::Pass);
    }

    #[test]
    fn receipt_declared_paths_must_exist() {
        let dir = std::env::temp_dir().join(format!("hatch-verify-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("out.bin");
        std::fs::write(&file, b"x").unwrap();
        let ok = json!({"content":[{"type":"text","text": json!({"path": file.to_str().unwrap(), "bytes": 1}).to_string()}],"isError":false});
        assert_eq!(receipt_check("mcp_call", &ok.to_string()).verdict, Verdict::Pass);
        let missing = json!({"content":[{"type":"text","text": json!({"path": dir.join("nope.bin").to_str().unwrap()}).to_string()}],"isError":false});
        assert_eq!(receipt_check("mcp_call", &missing.to_string()).verdict, Verdict::Fail);
        let empty = dir.join("empty.bin");
        std::fs::write(&empty, b"").unwrap();
        let empty_json = json!({"path": empty.to_str().unwrap()});
        assert_eq!(receipt_check("mcp_call", &empty_json.to_string()).verdict, Verdict::Fail);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn artifact_contract_checks_declared_files() {
        let dir = std::env::temp_dir().join(format!("hatch-artifact-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"hi").unwrap();
        let payload = json!({"root": dir.to_str().unwrap(), "artifacts": ["a.txt"]});
        assert_eq!(
            verify_by_contract(EffectContract::LocalArtifact, "gen", &payload, "").verdict,
            Verdict::Pass
        );
        let payload = json!({"root": dir.to_str().unwrap(), "artifacts": ["missing.txt"]});
        assert_eq!(
            verify_by_contract(EffectContract::LocalArtifact, "gen", &payload, "").verdict,
            Verdict::Fail
        );
        // No declared artifacts: degrades to the receipt check.
        let payload = json!({});
        assert_eq!(
            verify_by_contract(EffectContract::LocalArtifact, "gen", &payload, "ok").verdict,
            Verdict::Pass
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn readonly_contract_needs_no_evidence() {
        let c = verify_by_contract(EffectContract::ReadOnly, "fetch_url", &json!({}), "");
        assert_eq!(c.verdict, Verdict::NotApplicable);
    }
}
