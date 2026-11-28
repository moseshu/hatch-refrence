//! Replay recorded pipeline responses without contacting a model or a service.

use crate::recording::Exchange;
use hatch_core::{Error, Result};
use hatch_llm::{scripted::FnModel, ChatModel, ChatRequest, ChatResponse, LlmError};
use regex::Regex;
use serde::Serialize;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Default)]
struct Cursor {
    position: usize,
    divergence: Option<usize>,
}

pub struct Tape {
    pub model: Arc<dyn ChatModel>,
    cursor: Arc<Mutex<Cursor>>,
    length: usize,
}

impl Tape {
    pub fn new(exchanges: Vec<Exchange>) -> Result<Self> {
        if exchanges.is_empty()
            || exchanges
                .iter()
                .any(|e| e.error.is_some() || e.response.is_none() || e.redacted_response)
        {
            return Err(Error::Invalid(
                "replay requires a complete unredacted pipeline response tape".into(),
            ));
        }
        let length = exchanges.len();
        let cursor = Arc::new(Mutex::new(Cursor::default()));
        let model_cursor = cursor.clone();
        let model = Arc::new(FnModel::new("eval-replay", move |req| {
            let mut state = model_cursor.lock().unwrap_or_else(|p| p.into_inner());
            let index = state.position;
            if state.divergence.is_none() {
                if let Some(expected) = exchanges.get(index) {
                    if expected_shape(expected) == request_shape(req) {
                        let response = expected.response.as_ref().expect("validated tape");
                        state.position += 1;
                        return Ok(ChatResponse {
                            content: response.content.clone(),
                            tool_calls: response.tool_calls.clone(),
                            usage: response.usage,
                            ..Default::default()
                        });
                    }
                }
            }
            state.divergence = Some(index);
            Err(LlmError::permanent(format!(
                "no comparable scripted response at request {}",
                index + 1
            )))
        }));
        Ok(Self {
            model,
            cursor,
            length,
        })
    }

    /// The first unmatched request, including a tape left partly unconsumed.
    pub fn divergence(&self) -> Option<usize> {
        let state = self.cursor.lock().unwrap_or_else(|p| p.into_inner());
        state
            .divergence
            .or_else(|| (state.position != self.length).then_some(state.position))
    }
}

// Both callers provide text already processed by the trace redactor.
fn turn_label(role: &str, text: &str) -> (String, String) {
    let anchor = if role != "user" {
        String::new()
    } else if text.starts_with("审批结果") {
        "approval".into()
    } else if text.starts_with(hatch_agent::completeness::NOTE_PREFIX) {
        "completeness".into()
    } else {
        normalize(text)
    };
    (role.into(), anchor)
}

fn expected_shape(exchange: &Exchange) -> Vec<(String, String)> {
    exchange
        .messages
        .iter()
        .map(|m| {
            turn_label(
                m["role"].as_str().unwrap_or_default(),
                m["text"].as_str().unwrap_or_default(),
            )
        })
        .collect()
}

fn request_shape(request: &ChatRequest) -> Vec<(String, String)> {
    request
        .messages
        .iter()
        .map(|m| {
            if m.role == hatch_llm::Role::User {
                turn_label(
                    m.role.as_str(),
                    &hatch_core::secrets::redact(&m.text_content()),
                )
            } else {
                turn_label(m.role.as_str(), "")
            }
        })
        .collect()
}

fn normalize(text: &str) -> String {
    static VOLATILE: OnceLock<Regex> = OnceLock::new();
    VOLATILE.get_or_init(|| Regex::new(
        r"(?:/private)?/tmp/\.tmp[[:alnum:]]+|(?:/[[:alnum:]_.-]+)*/var/folders/[^\s\x22]+?/T/\.tmp[[:alnum:]]+|\b(?:apr|cp|evt|tsk|sess|ses|snap|turn)_[[:xdigit:]]{16}\b"
    ).expect("static regex")).replace_all(text, "<run-local>").into_owned()
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct Difference {
    pub stage: &'static str,
    pub request: Option<usize>,
}

fn evidence(exchange: &Exchange, role: &str, prefix: Option<&str>) -> Vec<String> {
    exchange
        .messages
        .iter()
        .filter_map(|message| {
            if message["role"].as_str()? != role {
                return None;
            }
            let text = message["text"].as_str()?;
            prefix
                .is_none_or(|prefix| text.starts_with(prefix))
                .then(|| normalize(text))
        })
        .collect()
}

fn context(dir: &Path, exchange: &Exchange) -> Result<Vec<String>> {
    exchange
        .contexts
        .iter()
        .map(|key| {
            if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(Error::Invalid("invalid replay context reference".into()));
            }
            Ok(normalize(&std::fs::read_to_string(
                dir.join("model-contexts").join(format!("{key}.txt")),
            )?))
        })
        .collect()
}

fn calls(exchange: &Exchange) -> Vec<(String, Value)> {
    exchange
        .response
        .iter()
        .flat_map(|response| &response.tool_calls)
        .map(|call| (call.name.clone(), normalize_value(call.arguments.clone())))
        .collect()
}

fn normalize_value(value: Value) -> Value {
    match value {
        Value::String(s) => Value::String(normalize(&s)),
        Value::Array(a) => Value::Array(a.into_iter().map(normalize_value).collect()),
        Value::Object(o) => Value::Object(
            o.into_iter()
                .map(|(k, v)| (k, normalize_value(v)))
                .collect(),
        ),
        other => other,
    }
}

pub fn compare(
    baseline: (&Path, &[Exchange], &Value),
    candidate: (&Path, &[Exchange], &Value),
    divergence: Option<usize>,
) -> Result<Option<Difference>> {
    let (left_dir, left, left_report) = baseline;
    let (right_dir, right, right_report) = candidate;
    for (index, (a, b)) in left.iter().zip(right).enumerate() {
        let stage = if a.tools != b.tools
            || a.settings != b.settings
            || context(left_dir, a)? != context(right_dir, b)?
        {
            "context"
        } else if evidence(a, "user", Some("审批结果")) != evidence(b, "user", Some("审批结果"))
        {
            "approval"
        } else if evidence(a, "tool", None) != evidence(b, "tool", None) {
            "readback"
        } else if divergence == Some(index)
            || a.workload != b.workload
            || expected_shape(a) != expected_shape(b)
        {
            "branch"
        } else if calls(a) != calls(b) {
            "tool_request"
        } else if a.response.as_ref().and_then(|r| r.content.as_deref())
            != b.response.as_ref().and_then(|r| r.content.as_deref())
        {
            "answer"
        } else {
            continue;
        };
        return Ok(Some(Difference {
            stage,
            request: Some(index + 1),
        }));
    }
    if left.len() != right.len() || divergence.is_some() {
        return Ok(Some(Difference {
            stage: "branch",
            request: Some(divergence.unwrap_or_else(|| left.len().min(right.len())) + 1),
        }));
    }
    for (stage, project) in [
        ("approval", approvals as fn(&Value) -> Value),
        ("readback", web_observations),
        ("final_state", final_state),
    ] {
        if project(left_report) != project(right_report) {
            return Ok(Some(Difference {
                stage,
                request: None,
            }));
        }
    }
    Ok(None)
}

fn approvals(report: &Value) -> Value {
    per_task(report, |task| {
        let mut approvals = normalize_value(task["approvals"].clone());
        if let Some(entries) = approvals.as_array_mut() {
            // The queue uses run-local IDs to break timestamp ties.
            entries.sort_by_cached_key(Value::to_string);
        }
        approvals
    })
}

fn web_observations(report: &Value) -> Value {
    per_task(report, |task| normalize_value(task["web"].clone()))
}

fn per_task(report: &Value, project: impl Fn(&Value) -> Value) -> Value {
    Value::Array(
        report["tasks"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|task| Value::Array(vec![normalize_value(task["id"].clone()), project(task)]))
            .collect(),
    )
}

fn final_state(report: &Value) -> Value {
    let checks: Vec<_> = report["checks"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|check| json!([check["name"], check["passed"]]))
        .collect();
    normalize_value(json!({"checks": checks, "outputs": report["outputs"]}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::Response;
    use hatch_llm::{ChatMessage, Usage};

    fn exchange(messages: Vec<Value>) -> Exchange {
        Exchange {
            router_id: 1,
            router_call_limit: None,
            workload: "main".into(),
            model: "script".into(),
            contexts: vec![],
            messages,
            tools: vec![],
            settings: Value::Null,
            response: Some(Response {
                content: Some("done".into()),
                tool_calls: vec![],
                usage: Usage::default(),
            }),
            error: None,
            redacted_response: false,
        }
    }

    #[test]
    fn stops_replaying_when_the_scripted_branch_changes() {
        let tape = Tape::new(vec![exchange(vec![json!({"role":"user","text":"first"})])]).unwrap();
        let next = ChatRequest::new(vec![ChatMessage::user("different task")]);
        assert!(tape.model.chat(&next).is_err());
        assert_eq!(tape.divergence(), Some(0));
    }

    #[test]
    fn matches_a_live_user_message_against_its_redacted_recording() {
        // A shared separator can cause the redactor to find another match on a
        // second pass, so use the same single-pass representation as the trace.
        let prompt = "本次记录编号：4111111111111111 4111111111111111。";
        let recorded = hatch_core::secrets::redact(prompt);
        assert_ne!(recorded, prompt);
        let tape = Tape::new(vec![exchange(vec![json!({"role":"user","text":recorded})])]).unwrap();
        let request = ChatRequest::new(vec![ChatMessage::user(prompt)]);
        assert!(tape.model.chat(&request).is_ok());
        assert_eq!(tape.divergence(), None);
    }

    #[test]
    fn identifies_a_planted_readback_difference_before_the_final_state() {
        let left = [exchange(vec![json!({"role":"tool","text":"original"})])];
        let right = [exchange(vec![json!({"role":"tool","text":"changed"})])];
        let report = json!({"checks":[]});
        let dir = Path::new(".");
        for _ in 0..2 {
            assert_eq!(
                compare((dir, &left, &report), (dir, &right, &report), None).unwrap(),
                Some(Difference {
                    stage: "readback",
                    request: Some(1)
                }),
            );
        }
    }

    #[test]
    fn approval_reports_ignore_order_but_keep_content_and_multiplicity() {
        let baseline = json!({
            "tasks": [{
                "id": "fresh-value",
                "approvals": [
                    {"kind": "fetch_url", "title": "Read source", "status": "executed"},
                    {"kind": "write_file", "title": "Write report", "status": "executed"}
                ]
            }]
        });
        let mut candidate = baseline.clone();
        candidate["tasks"][0]["approvals"]
            .as_array_mut()
            .unwrap()
            .reverse();
        let dir = Path::new(".");
        let difference = |candidate: &Value| {
            compare((dir, &[], &baseline), (dir, &[], candidate), None).unwrap()
        };
        assert_eq!(difference(&candidate), None);

        let approval_difference = Some(Difference {
            stage: "approval",
            request: None,
        });
        for (field, value) in [
            ("kind", "replace_file"),
            ("title", "Write another report"),
            ("status", "rejected"),
        ] {
            let mut changed = candidate.clone();
            changed["tasks"][0]["approvals"][0][field] = json!(value);
            assert_eq!(difference(&changed), approval_difference, "{field}");
        }
        let entries = candidate["tasks"][0]["approvals"].as_array_mut().unwrap();
        entries.push(entries[0].clone());
        assert_eq!(difference(&candidate), approval_difference);
    }
}
