//! The tool registry every planner and turn loop sees.
//!
//! One construction, shared by the chat turn loop (`setup_turn`), the
//! product API, and the `hatch plan` commands — so the planner validates
//! a plan against exactly the registry the executor will run it with. A
//! step naming a tool this registry cannot resolve is a broken plan, so
//! the planner is given the FULL registry here, not the Tool Bridge's
//! hot set: the bridge only changes what the turn loop *lists* to the
//! model (`Agent::tool_specs`); the registry stays the authority, and
//! `parse_plan_json` validates against the same registry.
//!
//! Sources, in order: built-in general tools (memory / mail / calendar /
//! file / shell / web) → browser tools → MCP tools from connected servers
//! (`mcp_<server>_<tool>`) → connector tools (`connector_<name>_<op>`) →
//! artifact/widget tools → skill tools (`list_skills` / `load_skill`,
//! never filtered out). An active skill's `tools` allowlist narrows the
//! registry for the turn — and it also applies to MCP tools, which are
//! added before the filter. A failed MCP server or a bad connector
//! manifest only warns; the registry keeps the tools that did load.

use crate::app::App;
use anyhow::Result;
use hatch_skills::Skill;
use hatch_tools::builtin::browser::add_browser_tools;
use hatch_tools::builtin::skills::add_skill_tools;
use hatch_tools::builtin::{add_general_tools, default_registry};
use hatch_tools::{Registry, Workspace};
use std::path::Path;
use std::sync::Arc;

/// Everything a caller needs from the registry construction: the
/// registry itself, the resolved workspace (if any), the active skills'
/// tool allowlist (if any skill narrows the registry), and the active
/// skills (for their context bodies).
pub(crate) struct ToolSet {
    pub tools: Registry,
    pub workspace: Option<Workspace>,
    pub skill_tools: Option<Vec<String>>,
    pub active_skills: Vec<Skill>,
}

/// Build the full tool registry for this process (see the module docs).
pub(crate) fn tool_registry(app: &App, workspace: Option<&Path>) -> Result<ToolSet> {
    let general = app.general_tools(workspace)?;
    let mut tools = add_general_tools(default_registry(), &general);
    if let Some(b) = app.browser() {
        tools = add_browser_tools(tools, b);
    }
    // MCP tools: connected once per process; a failed server only warns.
    // Added before the skill filter below so an active skill allowlist
    // also applies to them.
    let mcp = app.mcp();
    for w in &mcp.warnings {
        eprintln!("（{w}）");
    }
    for (server, info) in &mcp.tools {
        if let Some(client) = mcp.clients.lock().unwrap().get(server).cloned() {
            tools = tools.with(hatch_mcp::mcp_tool(server, info.clone(), client));
        }
    }
    // Connector tools: installed manifests under $HATCH_HOME/connectors.
    // A bad manifest only warns (see connector_tools); side-effecting ops
    // go through the approval queue like any other tool.
    for t in app.connector_tools() {
        tools = tools.with(t);
    }
    // Artifact/widget tools: show_options / create_artifact /
    // share_artifact.
    for t in app.artifact_tools() {
        tools = tools.with(t);
    }
    // Skills: list/load tools, plus active skills as a context layer.
    // A skill's `tools` allowlist narrows the registry for this turn;
    // list_skills / load_skill are never filtered out.
    let skill_reg = Arc::new(app.skill_registry());
    tools = add_skill_tools(tools, skill_reg, &app.home);
    let active = app.active_skills();
    let mut skill_tools: Option<Vec<String>> = None;
    if !active.is_empty() {
        let mut allowed: Vec<String> = Vec::new();
        for s in &active {
            for t in &s.tools {
                if !allowed.contains(t) {
                    allowed.push(t.clone());
                }
            }
        }
        let names = active
            .iter()
            .map(|s| s.name.clone())
            .collect::<Vec<_>>()
            .join("、");
        if allowed.is_empty() {
            eprintln!("（已加载技能：{names}）");
        } else {
            skill_tools = Some(allowed.clone());
            let mut keep: Vec<&str> = allowed.iter().map(String::as_str).collect();
            keep.extend(["list_skills", "load_skill"]);
            tools = tools.filtered(&keep);
            eprintln!(
                "（已加载技能：{names}；工具限定为：{}）",
                allowed.join("、")
            );
        }
    }
    Ok(ToolSet {
        workspace: general.workspace,
        tools,
        skill_tools,
        active_skills: active,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(reg: &Registry) -> Vec<String> {
        reg.specs().iter().map(|s| s.name.clone()).collect()
    }

    #[test]
    fn registry_has_skill_and_artifact_tools() {
        let home = tempfile::tempdir().unwrap();
        let app = App::open(Some(home.path())).unwrap();
        let set = tool_registry(&app, None).unwrap();
        let n = names(&set.tools);
        // Skill tools are always resident.
        assert!(n.contains(&"list_skills".to_string()), "{n:?}");
        assert!(n.contains(&"load_skill".to_string()), "{n:?}");
        // Artifact tools are part of the full registry the planner sees.
        assert!(n.contains(&"create_artifact".to_string()), "{n:?}");
        assert!(n.contains(&"show_options".to_string()), "{n:?}");
        // Built-ins from the default registry are present.
        assert!(n.contains(&"memory_search".to_string()), "{n:?}");
        assert!(n.contains(&"approval_status".to_string()), "{n:?}");
        // No active skills in a fresh home: no allowlist narrowing.
        assert!(set.skill_tools.is_none());
        assert!(set.active_skills.is_empty());
        // File / shell tools are workspace-scoped: they appear only
        // when a workspace is given.
        assert!(!n.contains(&"fs_read".to_string()), "{n:?}");
        let ws = tempfile::tempdir().unwrap();
        let set = tool_registry(&app, Some(ws.path())).unwrap();
        let n = names(&set.tools);
        assert!(n.contains(&"fs_read".to_string()), "{n:?}");
        assert!(n.contains(&"shell_run".to_string()), "{n:?}");
        assert!(set.workspace.is_some());
        // The MCP loop is a no-op with no configured server: the registry
        // still builds. A live MCP server is never started in tests; the
        // planner-level test (hatch-plan) covers an `mcp_*` name in the
        // registry with a fake entry instead.
    }
}
