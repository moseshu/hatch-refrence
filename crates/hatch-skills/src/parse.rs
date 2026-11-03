//! Hand-rolled frontmatter parser for `SKILL.md` files (no YAML dep).
//!
//! Expected layout:
//!
//! ```text
//! ---
//! name: my-skill
//! description: What this skill does.
//! tools: [fs_read, memory_search]
//! scope: chat
//! version: 1
//! ---
//! # Markdown body...
//! ```
//!
//! `name` and `description` are required; `tools`, `scope` and `version`
//! are optional (`scope` defaults to `"chat"`, `version` to `"1"`).

use crate::{Skill, MAX_SKILL_BYTES};
use hatch_core::{Error, Result};
use std::path::Path;

/// A skill name is `[A-Za-z0-9_-]+`.
pub fn valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::Invalid(msg.into())
}

/// Parse a `SKILL.md` file's contents into a [`Skill`].
///
/// `path` is recorded on the returned skill for diagnostics; the file is
/// not read here — callers pass the already-read text.
pub fn parse_skill(text: &str, path: &Path) -> Result<Skill> {
    if text.len() > MAX_SKILL_BYTES {
        return Err(invalid(format!(
            "skill file exceeds {MAX_SKILL_BYTES} bytes (got {} bytes)",
            text.len()
        )));
    }

    // Opening fence: exactly `---` on the first line.
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
        .ok_or_else(|| invalid("skill file must start with a `---` frontmatter fence"))?;

    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut tools: Option<Vec<String>> = None;
    let mut scope: Option<String> = None;
    let mut version: Option<String> = None;

    // Walk the frontmatter lines with exact byte offsets (split_inclusive
    // keeps each line's terminator, so offsets stay correct for \n and \r\n).
    let mut offset = 0;
    let mut body: Option<&str> = None;
    for chunk in rest.split_inclusive('\n') {
        let line = chunk.trim_end_matches(['\n', '\r']);
        offset += chunk.len();
        if line == "---" {
            body = Some(&rest[offset..]);
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        let (key, value) = line.split_once(':').ok_or_else(|| {
            invalid(format!(
                "frontmatter line has no `key: value` form: {line:?}"
            ))
        })?;
        let key = key.trim();
        let value = unquote(value.trim());
        match key {
            "name" => {
                if name.is_some() {
                    return Err(invalid("duplicate frontmatter key: name"));
                }
                name = Some(value.to_string());
            }
            "description" => {
                if description.is_some() {
                    return Err(invalid("duplicate frontmatter key: description"));
                }
                description = Some(value.to_string());
            }
            "tools" => {
                if tools.is_some() {
                    return Err(invalid("duplicate frontmatter key: tools"));
                }
                tools = Some(parse_tools(value));
            }
            "scope" => {
                if scope.is_some() {
                    return Err(invalid("duplicate frontmatter key: scope"));
                }
                scope = Some(value.to_string());
            }
            "version" => {
                if version.is_some() {
                    return Err(invalid("duplicate frontmatter key: version"));
                }
                version = Some(value.to_string());
            }
            _ => {
                // Unknown keys are ignored for forward compatibility.
            }
        }
    }

    let body = body.ok_or_else(|| invalid("skill file has no closing `---` frontmatter fence"))?;
    if body.trim().is_empty() {
        return Err(invalid("skill file has an empty body"));
    }

    let name = name.ok_or_else(|| invalid("skill frontmatter is missing required key: name"))?;
    if !valid_skill_name(&name) {
        return Err(invalid(format!(
            "invalid skill name {name:?}: use only letters, digits, `_` and `-`"
        )));
    }
    let description = description
        .ok_or_else(|| invalid("skill frontmatter is missing required key: description"))?;
    if description.trim().is_empty() {
        return Err(invalid("skill description must not be empty"));
    }

    Ok(Skill {
        name,
        description,
        tools: tools.unwrap_or_default(),
        scope: scope.unwrap_or_else(|| "chat".to_string()),
        version: version.unwrap_or_else(|| "1".to_string()),
        body: body.to_string(),
        path: path.to_path_buf(),
    })
}

/// Strip one pair of matching surrounding quotes (`"` or `'`), if present.
fn unquote(value: &str) -> &str {
    if value.len() >= 2 {
        let first = value.as_bytes()[0];
        let last = value.as_bytes()[value.len() - 1];
        if (first == b'"' || first == b'\'') && first == last {
            return &value[1..value.len() - 1];
        }
    }
    value
}

/// Parse a `tools` value: `[fs_read, memory_search]`, `fs_read, memory_search`,
/// a single tool, or empty.
fn parse_tools(value: &str) -> Vec<String> {
    let value = value.trim();
    let inner = value
        .strip_prefix('[')
        .and_then(|v| v.strip_suffix(']'))
        .unwrap_or(value);
    inner
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn path() -> &'static Path {
        Path::new("skills/demo/SKILL.md")
    }

    fn valid() -> String {
        "---\nname: demo\ndescription: A demo skill.\ntools: [fs_read, memory_search]\n---\n# Demo\n\nDo things.\n"
            .to_string()
    }

    #[test]
    fn parses_valid_skill() {
        let skill = parse_skill(&valid(), path()).unwrap();
        assert_eq!(skill.name, "demo");
        assert_eq!(skill.description, "A demo skill.");
        assert_eq!(skill.tools, ["fs_read", "memory_search"]);
        assert_eq!(skill.scope, "chat");
        assert_eq!(skill.version, "1");
        assert!(skill.body.contains("# Demo"));
        assert_eq!(skill.path, path());
    }

    #[test]
    fn missing_opening_fence_rejected() {
        let err = parse_skill("name: demo\ndescription: x\n---\nBody\n", path()).unwrap_err();
        assert!(matches!(err, Error::Invalid(_)));
        assert!(err.to_string().contains("must start with"));
    }

    #[test]
    fn missing_closing_fence_rejected() {
        let err = parse_skill("---\nname: demo\ndescription: x\n", path()).unwrap_err();
        assert!(matches!(err, Error::Invalid(_)));
        assert!(err.to_string().contains("no closing"));
    }

    #[test]
    fn missing_name_rejected() {
        let err = parse_skill("---\ndescription: x\n---\nBody\n", path()).unwrap_err();
        assert!(err.to_string().contains("missing required key: name"));
    }

    #[test]
    fn missing_description_rejected() {
        let err = parse_skill("---\nname: demo\n---\nBody\n", path()).unwrap_err();
        assert!(err
            .to_string()
            .contains("missing required key: description"));
    }

    #[test]
    fn invalid_name_rejected() {
        for bad in ["has space", "has/slash", "has.dot", ""] {
            let text = format!("---\nname: {bad}\ndescription: x\n---\nBody\n");
            let err = parse_skill(&text, path()).unwrap_err();
            assert!(err.to_string().contains("invalid skill name"), "{bad:?}");
        }
    }

    #[test]
    fn duplicate_key_rejected() {
        let err =
            parse_skill("---\nname: a\nname: b\ndescription: x\n---\nBody\n", path()).unwrap_err();
        assert!(err.to_string().contains("duplicate"));
        let err = parse_skill(
            "---\nname: a\ndescription: x\nscope: chat\nscope: agent\n---\nBody\n",
            path(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn empty_body_rejected() {
        let err = parse_skill("---\nname: demo\ndescription: x\n---\n", path()).unwrap_err();
        assert!(err.to_string().contains("empty body"));
        let err = parse_skill("---\nname: demo\ndescription: x\n---\n   \n", path()).unwrap_err();
        assert!(err.to_string().contains("empty body"));
    }

    #[test]
    fn oversized_skill_rejected() {
        let mut text = valid();
        text.push_str(&"x".repeat(MAX_SKILL_BYTES));
        let err = parse_skill(&text, path()).unwrap_err();
        assert!(err.to_string().contains("exceeds"));
    }

    #[test]
    fn exactly_max_bytes_accepted() {
        // Body padded so the whole file is exactly MAX_SKILL_BYTES.
        let mut text = valid();
        let pad = MAX_SKILL_BYTES - text.len();
        text.push_str(&"x".repeat(pad));
        assert_eq!(text.len(), MAX_SKILL_BYTES);
        assert!(parse_skill(&text, path()).is_ok());
    }

    #[test]
    fn tools_bracket_and_comma_forms() {
        let bracket = parse_skill(
            "---\nname: a\ndescription: x\ntools: [fs_read, memory_search]\n---\nBody\n",
            path(),
        )
        .unwrap();
        assert_eq!(bracket.tools, ["fs_read", "memory_search"]);

        let comma = parse_skill(
            "---\nname: a\ndescription: x\ntools: fs_read, memory_search\n---\nBody\n",
            path(),
        )
        .unwrap();
        assert_eq!(comma.tools, ["fs_read", "memory_search"]);

        let single = parse_skill(
            "---\nname: a\ndescription: x\ntools: fs_read\n---\nBody\n",
            path(),
        )
        .unwrap();
        assert_eq!(single.tools, ["fs_read"]);

        let empty = parse_skill(
            "---\nname: a\ndescription: x\ntools: []\n---\nBody\n",
            path(),
        )
        .unwrap();
        assert!(empty.tools.is_empty());

        let absent = parse_skill("---\nname: a\ndescription: x\n---\nBody\n", path()).unwrap();
        assert!(absent.tools.is_empty());
    }

    #[test]
    fn defaults_and_overrides() {
        let skill = parse_skill(
            "---\nname: a\ndescription: x\nscope: agent\nversion: 2\n---\nBody\n",
            path(),
        )
        .unwrap();
        assert_eq!(skill.scope, "agent");
        assert_eq!(skill.version, "2");
    }

    #[test]
    fn description_with_colon_kept() {
        let skill = parse_skill(
            "---\nname: a\ndescription: Does x: and y\n---\nBody\n",
            path(),
        )
        .unwrap();
        assert_eq!(skill.description, "Does x: and y");
    }

    #[test]
    fn crlf_file_parses() {
        let text = "---\r\nname: demo\r\ndescription: x\r\n---\r\nBody\r\n";
        let skill = parse_skill(text, path()).unwrap();
        assert_eq!(skill.name, "demo");
        assert_eq!(skill.body, "Body\r\n");
    }
}
