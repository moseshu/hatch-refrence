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

    #[test]
    fn environment_block_reporting_rule_present() {
        let b = behavior("Moses");
        assert!(b.contains("environment itself blocks you"), "{b}");
        assert!(b.contains("what you already tried"), "{b}");
        assert!(b.contains("quote the original error"), "{b}");
        assert!(b.contains("provide or change"), "{b}");
    }

    #[test]
    fn preference_lookup_rule_present() {
        let b = behavior("Alex");
        assert!(b.contains("Check standing preferences before choosing for Alex"), "{b}");
        assert!(b.contains("goes out in their name"), "{b}");
        assert!(b.contains("check for applicable preferences before deciding"), "{b}");
        assert!(
            b.contains("Use `memory_search` only for choices that a standing preference could affect and that the available context does not resolve"),
            "{b}"
        );
        assert!(b.contains("When they affect a proposal or write, check the actual tool arguments before each submission"), "{b}");
        assert!(b.contains("Keep using applicable preferences already found when a later lookup returns no match"), "{b}");
        assert!(b.contains("Compare each applicable preference with its final field value"), "{b}");
        assert!(b.contains("If the preference specifies exact wording, copy it verbatim"), "{b}");
        assert!(b.contains("Fill every detail the request asked for"), "{b}");
    }
}
