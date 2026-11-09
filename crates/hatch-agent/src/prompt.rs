//! Behaviour spec. The text lives in `prompts/behavior_en.md` so it can be
//! reviewed like code; its content hash is the prompt version recorded with
//! every session, which makes prompt changes traceable in evaluations.

pub const BEHAVIOR_TEMPLATE: &str = include_str!("../../../prompts/behavior_en.md");

/// Strip the leading licence comment and fill placeholders.
pub fn behavior(user_name: &str) -> String {
    strip_header(BEHAVIOR_TEMPLATE).replace("{user_name}", user_name)
}

/// Remove a leading `<!-- ... -->` block (author header) from a prompt file.
pub fn strip_header(text: &str) -> &str {
    let t = text.trim_start();
    if t.starts_with("<!--") {
        if let Some(end) = t.find("-->") {
            return t[end + 3..].trim_start();
        }
    }
    t
}

pub fn prompt_version() -> String {
    hatch_core::ids::digest(&[BEHAVIOR_TEMPLATE])[..12].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_name_and_drops_header() {
        let b = behavior("Moses");
        assert!(b.starts_with("# Behaviour Spec"));
        assert!(b.contains("Moses's personal assistant"));
        assert!(!b.contains("{user_name}"));
        assert_eq!(prompt_version().len(), 12);
    }
}
