//! Exact-sequence assertions for eval worlds (OMB #6).
//!
//! Hatch's eval checks used to be result-state level (counts, statuses,
//! files): a run could take a completely different route and still pass
//! as long as the end state matched. These helpers pin the *trajectory*
//! instead, following the shape of OpenMausBot's `toolNames` /
//! `toolCalls` / `turnOrder` / `handoffTree` assertions
//! (`evals/scorers/assertions.ts`), re-expressed for hatch:
//!
//! * [`assert_tool_names`] — tool names only, in order (OMB `toolNames`);
//! * [`assert_tool_calls`] — tool names plus arguments, in order, with
//!   extra calls failing (OMB `toolCalls`);
//! * [`assert_turn_order`] — the order of agents acting, in order
//!   (OMB `turnOrder`);
//! * [`assert_handoff_tree`] — the delegation tree as
//!   (agent, depth, status) nodes, in spawn order (OMB `handoffTree`;
//!   hatch's `depth > 0` already says "has a parent", so there is no
//!   separate `hasParent` field);
//! * [`assert_subsequence`] — the expected names appear in order, extra
//!   calls allowed: the live-mode variant, for when a real model may
//!   take extra steps on the way.
//!
//! Every helper is pure: it takes the recorded sequence and the expected
//! one and returns `(passed, detail)`. Expectations live in the world
//! spec (data), never in these functions.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One recorded or expected tool call. `arguments: None` on an *expected*
/// call matches any arguments (name only); on a recorded call it is
/// always `Some`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SeqCall {
    pub name: String,
    #[serde(default)]
    pub arguments: Option<Value>,
}

impl SeqCall {
    pub fn named(name: &str) -> Self {
        SeqCall {
            name: name.into(),
            arguments: None,
        }
    }

    pub fn with_args(name: &str, arguments: Value) -> Self {
        SeqCall {
            name: name.into(),
            arguments: Some(arguments),
        }
    }
}

/// One node of a delegation tree, in spawn order.
#[derive(Clone, Debug, PartialEq)]
pub struct HandoffNode {
    pub agent: String,
    pub depth: usize,
    pub status: String,
}

fn fmt_names(names: &[String]) -> String {
    if names.is_empty() {
        "（无调用）".into()
    } else {
        names.join(" → ")
    }
}

/// Tool names only, in order. Extra calls fail, missing calls fail.
pub fn assert_tool_names(actual: &[String], expected: &[String]) -> (bool, String) {
    let mut mismatches: Vec<String> = expected
        .iter()
        .enumerate()
        .filter_map(|(i, want)| match actual.get(i) {
            Some(got) if got == want => None,
            Some(got) => Some(format!("第 {} 次调用：期望 {want}，实际 {got}", i + 1)),
            None => Some(format!("第 {} 次调用：期望 {want}，实际无调用", i + 1)),
        })
        .collect();
    if actual.len() > expected.len() {
        mismatches.push(format!(
            "多出 {} 次调用：{}",
            actual.len() - expected.len(),
            actual[expected.len()..].join(", ")
        ));
    }
    if mismatches.is_empty() {
        (true, format!("{} 次调用，顺序一致", actual.len()))
    } else {
        (
            false,
            format!(
                "期望 {}；实际 {}。{}",
                fmt_names(expected),
                fmt_names(actual),
                mismatches.join("；")
            ),
        )
    }
}

/// Tool names plus arguments, in order. An expected call with
/// `arguments: None` matches on the name alone; otherwise the arguments
/// must be deep-equal. Extra calls fail.
pub fn assert_tool_calls(actual: &[SeqCall], expected: &[SeqCall]) -> (bool, String) {
    let mut mismatches: Vec<String> = Vec::new();
    for (i, want) in expected.iter().enumerate() {
        match actual.get(i) {
            None => mismatches.push(format!(
                "第 {} 次调用：期望 {}，实际无调用",
                i + 1,
                want.name
            )),
            Some(got) => {
                if got.name != want.name {
                    mismatches.push(format!(
                        "第 {} 次调用：期望 {}，实际 {}",
                        i + 1,
                        want.name,
                        got.name
                    ));
                } else if let Some(want_args) = &want.arguments {
                    let got_args = got.arguments.as_ref();
                    if got_args != Some(want_args) {
                        mismatches.push(format!(
                            "第 {} 次调用 {}：期望参数 {want_args}，实际参数 {}",
                            i + 1,
                            want.name,
                            got_args
                                .map(|v| v.to_string())
                                .unwrap_or_else(|| "无".into())
                        ));
                    }
                }
            }
        }
    }
    if actual.len() > expected.len() {
        mismatches.push(format!(
            "多出 {} 次调用：{}",
            actual.len() - expected.len(),
            actual[expected.len()..]
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if mismatches.is_empty() {
        (true, format!("{} 次调用（含参数）全部一致", actual.len()))
    } else {
        (
            false,
            format!(
                "期望 {}；实际 {}。{}",
                expected
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(" → "),
                actual
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(" → "),
                mismatches.join("；")
            ),
        )
    }
}

/// The order of agents acting, in order. Used on sub-agent spawn order:
/// the parent's own calls are pinned by [`assert_tool_names`].
pub fn assert_turn_order(actual: &[String], expected: &[String]) -> (bool, String) {
    if actual == expected {
        (true, fmt_names(actual))
    } else {
        (
            false,
            format!("期望 {}；实际 {}", fmt_names(expected), fmt_names(actual)),
        )
    }
}

/// The delegation tree as (agent, depth, status) nodes, in spawn order.
pub fn assert_handoff_tree(actual: &[HandoffNode], expected: &[HandoffNode]) -> (bool, String) {
    let fmt = |nodes: &[HandoffNode]| {
        nodes
            .iter()
            .map(|n| format!("{}@{}:{}", n.agent, n.depth, n.status))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if actual == expected {
        (true, format!("{} 个节点一致", actual.len()))
    } else {
        (
            false,
            format!("期望 [{}]；实际 [{}]", fmt(expected), fmt(actual)),
        )
    }
}

/// Expected names appear in order; extra calls are allowed. The
/// live-mode variant: a real model may take extra steps on the way.
pub fn assert_subsequence(actual: &[String], expected: &[String]) -> (bool, String) {
    let mut at = 0;
    for want in expected {
        match actual[at..].iter().position(|got| got == want) {
            Some(i) => at += i + 1,
            None => {
                return (
                    false,
                    format!(
                        "期望 {} 按顺序出现；实际 {}，在 {} 之后找不到 {want}",
                        fmt_names(expected),
                        fmt_names(actual),
                        if at == 0 {
                            "开头".to_string()
                        } else {
                            actual[at - 1].clone()
                        }
                    ),
                );
            }
        }
    }
    (
        true,
        format!("期望序列 {} 均按顺序出现", fmt_names(expected)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn tool_names_exact_match_and_mismatches() {
        let (ok, _) = assert_tool_names(&names(&["a", "b"]), &names(&["a", "b"]));
        assert!(ok);
        let (ok, d) = assert_tool_names(&names(&["a"]), &names(&["a", "b"]));
        assert!(!ok, "{d}");
        assert!(d.contains("第 2 次调用"), "{d}");
        let (ok, d) = assert_tool_names(&names(&["a", "b", "c"]), &names(&["a", "b"]));
        assert!(!ok, "{d}");
        assert!(d.contains("多出 1 次调用：c"), "{d}");
        let (ok, d) = assert_tool_names(&names(&["b", "a"]), &names(&["a", "b"]));
        assert!(!ok, "{d}");
        assert!(d.contains("第 1 次调用：期望 a，实际 b"), "{d}");
        let (ok, _) = assert_tool_names(&[], &[]);
        assert!(ok);
        let (ok, _) = assert_tool_names(&[], &names(&["a"]));
        assert!(!ok);
    }

    #[test]
    fn tool_calls_check_arguments_and_extra_calls() {
        let actual = vec![
            SeqCall::with_args(
                "fs_grep",
                json!({"pattern": "TODO", "glob": "services/auth/**"}),
            ),
            SeqCall::named("shell_run"),
        ];
        let expected = vec![
            SeqCall::with_args(
                "fs_grep",
                json!({"pattern": "TODO", "glob": "services/auth/**"}),
            ),
            SeqCall::named("shell_run"),
        ];
        assert!(assert_tool_calls(&actual, &expected).0);
        // Argument mismatch fails, name-only expected passes any args.
        let wrong = vec![SeqCall::with_args(
            "fs_grep",
            json!({"pattern": "TODO", "glob": "services/other/**"}),
        )];
        let (ok, d) = assert_tool_calls(&wrong, &expected[..1]);
        assert!(!ok, "{d}");
        assert!(d.contains("期望参数"), "{d}");
        let (ok, _) = assert_tool_calls(&wrong, &[SeqCall::named("fs_grep")]);
        assert!(ok);
        // Extra call fails even when the prefix matches.
        let (ok, d) = assert_tool_calls(&actual, &expected[..1]);
        assert!(!ok, "{d}");
        assert!(d.contains("多出 1 次调用"), "{d}");
    }

    #[test]
    fn turn_order_and_handoff_tree() {
        let (ok, _) = assert_turn_order(
            &names(&["explore", "explore"]),
            &names(&["explore", "explore"]),
        );
        assert!(ok);
        let (ok, d) = assert_turn_order(
            &names(&["execute", "explore"]),
            &names(&["explore", "execute"]),
        );
        assert!(!ok, "{d}");
        let node = |a: &str, d: usize, s: &str| HandoffNode {
            agent: a.into(),
            depth: d,
            status: s.into(),
        };
        let actual = vec![
            node("execute", 1, "completed"),
            node("explore", 2, "rejected"),
        ];
        assert!(assert_handoff_tree(&actual, &actual).0);
        let (ok, d) = assert_handoff_tree(&actual, &[node("execute", 1, "completed")]);
        assert!(!ok, "{d}");
        assert!(d.contains("rejected"), "{d}");
    }

    #[test]
    fn subsequence_allows_extras_but_not_reorder() {
        let (ok, _) = assert_subsequence(&names(&["a", "x", "b", "c"]), &names(&["a", "b"]));
        assert!(ok);
        let (ok, _) = assert_subsequence(&names(&["b", "a"]), &names(&["a", "b"]));
        assert!(!ok);
        let (ok, _) = assert_subsequence(&names(&["a"]), &names(&["a", "b"]));
        assert!(!ok);
        let (ok, _) = assert_subsequence(&names(&["a", "b"]), &[]);
        assert!(ok);
    }
}
