//! The tool bridge: list only the hot tools, hide the rest behind search.
//!
//! Follows the OpenMausBot bridge (`server/drivers/agents-bridge.ts`,
//! read-only reference): a turn lists the tools it needs most turns plus
//! `find_tool` / `use_tool`; the rest are searchable by BM25. What a turn
//! may call does not change — the registry stays the authority; the bridge
//! only changes the listing, so a direct call to a hidden tool still works.
//!
//! Security invariant: a bridged call is never judged more permissively
//! than the same call made directly. `use_tool` routes through the same
//! dispatch path as a direct call, and the skill allowlist is re-checked
//! on the routed tool before execution (see `Agent::run`).

use hatch_llm::ToolSpec;
use hatch_memory::Bm25Index;
use hatch_tools::Registry;
use serde_json::{json, Value};

pub const FIND_TOOL: &str = "find_tool";
pub const USE_TOOL: &str = "use_tool";
/// Matches per search; each carries its full input schema.
pub const FIND_MATCHES_MAX: usize = 3;
/// Room for the hidden tools' names in find_tool's own description.
const BRIDGE_NAMES_MAX_CHARS: usize = 1_200;

pub const FIND_USAGE: &str = "find_tool needs { \"query\": \"what you want to do, in a few words, or the exact tool name\" }, e.g. \"schedule a meeting\".";
pub const USE_USAGE: &str = "use_tool needs { \"name\": \"the exact tool name\", \"arguments\": { ... } }; arguments is a JSON object written per that tool's input schema. Find the name and schema with find_tool first.";

/// Tools listed on every turn: the ones most turns need. `list_skills` /
/// `load_skill` are always listed too (a skill allowlist narrows the
/// registry, never the skill tools themselves).
///
/// Chosen from the registry as actually registered for a chat turn
/// (`default_registry` + general + browser + artifact + skill tools):
/// memory and mail are the assistant's core surfaces; shell and the file
/// tools are the workhorses for workspace work; web fetch/search cover
/// lookup; the two `propose_*` tools are outbound actions, listed so they
/// are always called by their own name; `approval_status` is how the
/// model checks the pending queue. `browser_open` is listed too: opening
/// a page is the read entry point for browsing, and it is approval-gated
/// (`fetch_web`), so listing it costs no safety. Everything else is
/// reachable through `find_tool`: `memory_explain` (diagnostic),
/// `calendar_free_slots` (scheduling detail), `fs_glob` (covered by
/// `fs_list` + `fs_grep`), the remaining `browser_*` tools, the artifact
/// tools (UI cards, rare in chat), MCP and connector tools (variable set).
pub const HOT_TOOLS: &[&str] = &[
    "memory_search",
    "memory_add",
    "mail_search",
    "propose_email",
    "calendar_list",
    "propose_event",
    "shell_run",
    "fs_read",
    "fs_list",
    "fs_grep",
    "fs_write",
    "fs_edit",
    "web_fetch",
    "web_search",
    "browser_open",
    "approval_status",
];

/// Whether a tool is always listed, whatever the hot set says.
fn is_always_listed(name: &str) -> bool {
    name == "list_skills" || name == "load_skill"
}

pub fn is_meta_call(name: &str) -> bool {
    name == FIND_TOOL || name == USE_TOOL
}

/// `a, b, c.` within max_chars, or `a, b, … and 12 more.` Names are never cut.
fn names_list(names: &[&str], max_chars: usize) -> String {
    if names.is_empty() {
        return "none.".into();
    }
    let mut text = String::new();
    let mut shown = 0;
    for name in names {
        let part = format!("{}{}", if shown > 0 { ", " } else { "" }, name);
        let last = shown == names.len() - 1;
        if text.len() + part.len() + if last { 1 } else { 24 } > max_chars {
            break;
        }
        text += &part;
        shown += 1;
    }
    if shown < names.len() {
        format!(
            "{}{}… and {} more.",
            text,
            if shown > 0 { ", " } else { "" },
            names.len() - shown
        )
    } else {
        format!("{text}.")
    }
}

fn find_tool_spec(hidden_names: &[&str]) -> ToolSpec {
    ToolSpec {
        name: FIND_TOOL.into(),
        description: format!(
            "Find tools that are not in the tool list. Describe what you want to do in a few words, or give the exact tool name. Each match returns its name, description, and full input schema; run it with {}. Tools findable this way: {}",
            USE_TOOL,
            names_list(hidden_names, BRIDGE_NAMES_MAX_CHARS)
        ),
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "query": { "type": "string", "description": "what you want to do, in a few words, or the exact tool name." } },
            "required": ["query"]
        }),
    }
}

fn use_tool_spec() -> ToolSpec {
    ToolSpec {
        name: USE_TOOL.into(),
        description: format!(
            "Run a tool found via {}, passing arguments by its exact name and input schema. It executes exactly like a direct call: same permissions, limits, and refusals, results returned as-is. Read the description and schema from {} before the first call.",
            FIND_TOOL, FIND_TOOL
        ),
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "name": { "type": "string", "description": "the exact tool name, as given by find_tool." },
                "arguments": { "type": "object", "additionalProperties": true, "description": "the tool input, written per the input schema from find_tool. May be omitted for tools without parameters." }
            },
            "required": ["name"]
        }),
    }
}

/// Registry specs narrowed by a skill allowlist (`list_skills` /
/// `load_skill` are never filtered out).
pub fn allowed_specs(tools: &Registry, allowlist: &Option<Vec<String>>) -> Vec<ToolSpec> {
    let mut specs = tools.specs();
    if let Some(allowed) = allowlist {
        specs.retain(|s| {
            s.name == "list_skills"
                || s.name == "load_skill"
                || allowed.iter().any(|a| a == &s.name)
        });
    }
    specs
}

/// The listed/hidden split of one turn's tool set.
pub struct ToolBridge {
    listed: Vec<ToolSpec>,
    hidden: Vec<ToolSpec>,
}

impl ToolBridge {
    /// Split `specs` (already narrowed by any skill allowlist) into listed
    /// and hidden. The two meta tools are appended to the listed set, so
    /// the listed set is hot tools + skill tools + `find_tool` + `use_tool`.
    pub fn from_specs(specs: Vec<ToolSpec>) -> Self {
        let mut listed = Vec::new();
        let mut hidden = Vec::new();
        for s in specs {
            if is_always_listed(&s.name) || HOT_TOOLS.contains(&s.name.as_str()) {
                listed.push(s);
            } else {
                hidden.push(s);
            }
        }
        let hidden_names: Vec<&str> = hidden.iter().map(|s| s.name.as_str()).collect();
        listed.push(find_tool_spec(&hidden_names));
        listed.push(use_tool_spec());
        ToolBridge { listed, hidden }
    }

    pub fn listed(&self) -> &[ToolSpec] {
        &self.listed
    }

    pub fn hidden(&self) -> &[ToolSpec] {
        &self.hidden
    }

    pub fn is_listed(&self, name: &str) -> bool {
        self.listed.iter().any(|s| s.name == name)
    }

    pub fn is_hidden(&self, name: &str) -> bool {
        self.hidden.iter().any(|s| s.name == name)
    }

    /// Whether the name is in this turn's profile at all (listed or hidden).
    pub fn contains(&self, name: &str) -> bool {
        self.is_listed(name) || self.is_hidden(name)
    }

    /// Search the hidden tools for `query`: an exact name wins, otherwise
    /// BM25 over name + description, best [`FIND_MATCHES_MAX`] first.
    /// A query naming a listed tool is not searched — it is called directly.
    pub fn find(&self, query: &str) -> Vec<&ToolSpec> {
        let q = query.trim();
        if q.is_empty() {
            return Vec::new();
        }
        if let Some(t) = self.hidden.iter().find(|s| s.name == q) {
            return vec![t];
        }
        let index = Bm25Index::build(
            self.hidden
                .iter()
                .map(|s| (s.name.as_str(), format!("{} {}", s.name, s.description))),
        );
        index
            .query(q, FIND_MATCHES_MAX)
            .iter()
            .filter_map(|(id, _)| self.hidden.iter().find(|s| &s.name == id))
            .collect()
    }
}

/// Render `find_tool` matches as JSON: name + description + full schema per
/// match, plus how to run one. No match means the query shared no word
/// with any hidden tool — say so and list the hidden names.
pub fn render_find_result(matches: &[&ToolSpec], hidden_names: &[&str]) -> String {
    if matches.is_empty() {
        return serde_json::to_string(&json!({
            "matches": [],
            "next": format!("No match. Try different wording, or use one of these names directly: {}. Tools in the tool list are called directly.", names_list(hidden_names, BRIDGE_NAMES_MAX_CHARS))
        }))
        .unwrap_or_else(|_| "No match.".into());
    }
    let list: Vec<Value> = matches
        .iter()
        .map(|s| json!({ "name": s.name, "description": s.description, "inputSchema": s.parameters }))
        .collect();
    serde_json::to_string(&json!({
        "matches": list,
        "next": format!("Run one of them with {USE_TOOL}({{\"name\": \"...\", \"arguments\": {{...}}}}), writing arguments per its inputSchema. Use it per its description, just like a listed tool.")
    }))
    .unwrap_or_else(|_| "No match.".into())
}

/// Parse a `use_tool` call into (tool name, arguments). Only the call's
/// shape is checked here: the tool reads its own arguments, as on a direct
/// call.
pub fn parse_use_call(args: &Value) -> Result<(String, Value), String> {
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    let input = args.get("arguments");
    if name.is_empty() || (input.is_some() && !input.unwrap().is_object()) {
        return Err(USE_USAGE.into());
    }
    Ok((
        name.to_string(),
        input.cloned().unwrap_or_else(|| json!({})),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, desc: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: desc.into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    fn sample_specs() -> Vec<ToolSpec> {
        vec![
            spec("memory_search", "搜索记忆"),
            spec("memory_explain", "查看某条记忆的来源与修订历史"),
            spec("shell_run", "执行 shell 命令"),
            spec("fs_glob", "按模式查找文件"),
            spec("list_skills", "列出已安装的技能"),
            spec("load_skill", "加载一个技能"),
            spec("browser_open", "打开一个网页"),
        ]
    }

    #[test]
    fn split_keeps_hot_and_skills_listed() {
        let b = ToolBridge::from_specs(sample_specs());
        let listed: Vec<&str> = b.listed().iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            listed,
            [
                "memory_search",
                "shell_run",
                "list_skills",
                "load_skill",
                "browser_open",
                "find_tool",
                "use_tool"
            ]
        );
        let hidden: Vec<&str> = b.hidden().iter().map(|s| s.name.as_str()).collect();
        assert_eq!(hidden, ["memory_explain", "fs_glob"]);
        assert!(b.is_listed("memory_search") && b.is_hidden("fs_glob"));
        assert!(!b.is_listed("fs_glob") && !b.is_hidden("memory_search"));
        assert!(b.is_listed("browser_open") && !b.is_hidden("browser_open"));
    }

    #[test]
    fn find_prefers_exact_name_then_bm25() {
        let b = ToolBridge::from_specs(sample_specs());
        assert_eq!(
            b.find("memory_explain")
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["memory_explain"]
        );
        let hits = b.find("查看记忆的来源");
        assert_eq!(hits[0].name, "memory_explain");
        let hits = b.find("按模式查找文件");
        assert_eq!(hits[0].name, "fs_glob");
        assert!(b.find("zzzqqq").is_empty());
        assert!(b.find("  ").is_empty());
    }

    #[test]
    fn browser_tools_reachable_on_default_turn() {
        // The browser set joins the default registry: `browser_open`
        // is listed directly, the rest are hidden but findable — so a
        // default turn can open a page and then act on it.
        let mut specs = sample_specs();
        specs.push(spec("browser_read", "读取当前页面的内容与元素编号"));
        specs.push(spec("browser_click", "点击页面上的元素"));
        specs.push(spec("browser_type", "向页面输入框输入文字"));
        let b = ToolBridge::from_specs(specs);
        assert!(b.is_listed("browser_open"));
        assert!(b.is_hidden("browser_click") && b.is_hidden("browser_read"));
        assert_eq!(
            b.find("browser_click")
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["browser_click"]
        );
        let hits = b.find("点击页面元素");
        assert_eq!(hits[0].name, "browser_click");
        // find_tool's own description must name the hidden browser
        // tools, or the model never learns they exist.
        let f = b.listed().iter().find(|s| s.name == FIND_TOOL).unwrap();
        assert!(f.description.contains("browser_click"), "{}", f.description);
        assert!(f.description.contains("browser_read"), "{}", f.description);
    }

    #[test]
    fn find_returns_at_most_three() {
        let mut specs = vec![spec("memory_search", "搜索记忆")];
        for i in 0..6 {
            specs.push(spec(&format!("tool_{i}"), "查找文件内容"));
        }
        let b = ToolBridge::from_specs(specs);
        assert!(b.find("查找文件").len() <= FIND_MATCHES_MAX);
    }

    #[test]
    fn find_tool_description_names_the_hidden_tools() {
        let b = ToolBridge::from_specs(sample_specs());
        let f = b.listed().iter().find(|s| s.name == FIND_TOOL).unwrap();
        assert!(
            f.description.contains("memory_explain"),
            "{}",
            f.description
        );
        assert!(f.description.contains("fs_glob"), "{}", f.description);
        // browser_open is hot now, so it must not appear in the hidden list.
        assert!(!f.description.contains("browser_open"), "{}", f.description);
    }

    #[test]
    fn names_list_never_cuts_a_name() {
        let names: Vec<&str> = (0..30).map(|_| "averylongtoolname").collect();
        let s = names_list(&names, 60);
        assert!(s.len() <= 60 + "… and 29 more.".len());
        assert!(s.contains("more."), "{s}");
        assert!(!s.contains("averylongtoolnam,"), "{s}");
    }

    #[test]
    fn parse_use_call_accepts_shape_only() {
        let (n, a) =
            parse_use_call(&json!({"name": "fs_glob", "arguments": {"pattern": "*.rs"}})).unwrap();
        assert_eq!(n, "fs_glob");
        assert_eq!(a, json!({"pattern": "*.rs"}));
        let (n, a) = parse_use_call(&json!({"name": "fs_glob"})).unwrap();
        assert_eq!((n, a), ("fs_glob".to_string(), json!({})));
        assert!(parse_use_call(&json!({})).is_err());
        assert!(parse_use_call(&json!({"name": "x", "arguments": "no"})).is_err());
        assert!(parse_use_call(&json!({"name": "  "})).is_err());
    }

    #[test]
    fn render_find_result_round_trips() {
        let b = ToolBridge::from_specs(sample_specs());
        let hidden_names: Vec<&str> = b.hidden().iter().map(|s| s.name.as_str()).collect();
        let out = render_find_result(&b.find("查看记忆的来源"), &hidden_names);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["matches"][0]["name"], "memory_explain");
        assert!(v["matches"][0]["inputSchema"].is_object());
        let out = render_find_result(&[], &hidden_names);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["matches"].as_array().unwrap().len(), 0);
        assert!(
            v["next"].as_str().unwrap().contains("memory_explain"),
            "{out}"
        );
    }
}
