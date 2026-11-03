//! Extract a JSON value from model output that may contain prose or code fences.

use serde_json::Value;

/// Best-effort: whole text → fenced block → outermost `{...}` / `[...]`.
pub fn extract_json(text: &str) -> Option<Value> {
    let t = text.trim();
    if let Ok(v) = serde_json::from_str::<Value>(t) {
        return Some(v);
    }
    if let Some(start) = t.find("```") {
        let rest = &t[start + 3..];
        let rest = rest.strip_prefix("json").unwrap_or(rest);
        if let Some(end) = rest.find("```") {
            if let Ok(v) = serde_json::from_str::<Value>(rest[..end].trim()) {
                return Some(v);
            }
        }
    }
    for (open, close) in [('{', '}'), ('[', ']')] {
        if let (Some(s), Some(e)) = (t.find(open), t.rfind(close)) {
            if s < e {
                if let Ok(v) = serde_json::from_str::<Value>(&t[s..=e]) {
                    return Some(v);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn variants() {
        assert_eq!(extract_json("{\"a\":1}"), Some(json!({"a":1})));
        assert_eq!(extract_json("结果如下：\n```json\n{\"a\":2}\n```\n完毕"), Some(json!({"a":2})));
        assert_eq!(extract_json("sure! {\"a\":3} hope it helps"), Some(json!({"a":3})));
        assert_eq!(extract_json("[1,2]"), Some(json!([1, 2])));
        assert_eq!(extract_json("no json here"), None);
    }
}
