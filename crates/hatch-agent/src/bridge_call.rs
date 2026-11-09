//! Routing for the bridge's meta tools, called from the turn loop.
//!
//! `find_tool` searches the hidden tools (rebuilt from the live allowlist,
//! so a skill loaded mid-turn is searchable at once). `use_tool` refuses
//! listed tools ("call it directly"), re-checks the skill allowlist on the
//! routed tool, then dispatches through the same path as a direct call —
//! gate, safety floor and executors all apply, so a bridged call can never
//! be more permissive than the same call made directly.

use crate::agent::Agent;
use crate::bridge::{
    allowed_specs, is_meta_call, parse_use_call, render_find_result, ToolBridge, FIND_TOOL,
    FIND_USAGE, USE_TOOL,
};
use crate::subagent;
use hatch_core::Trust;
use hatch_llm::ToolCall;
use hatch_tools::{dispatch_call, ToolContext};
use serde_json::Value;

/// Whether `name` is blocked by the skill allowlist. Meta tools
/// (list/load skills, find/use tool) are exempt — they are always offered;
/// the allowlist is enforced on the tool a meta call routes to instead.
/// Cancelling, pausing and resuming are part of managing delegations:
/// an allowlist that names a delegate tool also permits them.
pub(crate) fn allowlist_blocks(allowlist: &Option<Vec<String>>, name: &str) -> bool {
    let Some(a) = allowlist else {
        return false;
    };
    name != "list_skills"
        && name != "load_skill"
        && !is_meta_call(name)
        && !a.iter().any(|n| n == name)
        && !(subagent::is_cancel_call(name) && a.iter().any(|n| subagent::is_delegate_call(n)))
        && !(subagent::is_pause_call(name) && a.iter().any(|n| subagent::is_delegate_call(n)))
        && !(subagent::is_resume_call(name) && a.iter().any(|n| subagent::is_delegate_call(n)))
}

pub(crate) fn skill_refusal(allowlist: &Option<Vec<String>>, name: &str) -> String {
    let allowed = allowlist.as_ref().map(|a| a.join("、")).unwrap_or_default();
    format!("技能限制：当前已加载技能只允许使用以下工具：{allowed}。本次对 {name} 的调用被拒绝。")
}

/// Handle one `find_tool` / `use_tool` call.
///
/// Returns (content, trust, approval ids, reported name): a routed call is
/// reported under the tool it ran, as the bridge's caller would name it; a
/// refused call keeps the meta tool's own name.
pub(crate) fn bridge_call(
    agent: &Agent,
    call: &ToolCall,
    allowlist: &Option<Vec<String>>,
    ctx: &ToolContext,
) -> (String, Trust, Vec<String>, String) {
    let bridge = ToolBridge::from_specs(allowed_specs(agent.tools, allowlist));
    let own = call.name.clone();
    if call.name == FIND_TOOL {
        let q = call
            .arguments
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if q.is_empty() {
            return (FIND_USAGE.into(), Trust::System, Vec::new(), own);
        }
        if bridge.is_listed(q) {
            return (
                format!("{q} 在工具列表里，直接调用它。"),
                Trust::System,
                Vec::new(),
                own,
            );
        }
        let matches = bridge.find(q);
        let hidden_names: Vec<&str> = bridge.hidden().iter().map(|s| s.name.as_str()).collect();
        (
            render_find_result(&matches, &hidden_names),
            Trust::System,
            Vec::new(),
            own,
        )
    } else {
        match parse_use_call(&call.arguments) {
            Err(usage) => (usage, Trust::System, Vec::new(), own),
            Ok((name, args)) => {
                if bridge.is_listed(&name)
                    || subagent::is_delegate_call(&name)
                    || subagent::is_cancel_call(&name)
                    || subagent::is_pause_call(&name)
                    || subagent::is_resume_call(&name)
                {
                    (
                        format!(
                            "{name} 在工具列表里，直接调用它，不要经过 {USE_TOOL}。本次没有执行。"
                        ),
                        Trust::System,
                        Vec::new(),
                        own,
                    )
                } else if allowlist_blocks(allowlist, &name) {
                    (
                        skill_refusal(allowlist, &name),
                        Trust::System,
                        Vec::new(),
                        own,
                    )
                } else {
                    let inner = ToolCall {
                        id: call.id.clone(),
                        name: name.clone(),
                        arguments: args,
                    };
                    let out = dispatch_call(agent.tools, agent.executors, agent.gate, &inner, ctx);
                    // A name outside the profile is refused by dispatch as
                    // unknown, exactly as a direct call would be — and it
                    // keeps use_tool's own name, since nothing ran under it.
                    let reported = if bridge.contains(&name) { name } else { own };
                    (
                        out.content,
                        out.trust,
                        out.approval_id.into_iter().collect(),
                        reported,
                    )
                }
            }
        }
    }
}
