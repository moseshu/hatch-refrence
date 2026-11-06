//! Scopes for MCP clients: what a bearer token is allowed to do.
//!
//! A client token carries one or both of two scopes:
//!
//! * `read` — call read-only tools (`list_*`, `get_*`, `read_*`, ...),
//!   list tools/resources/prompts, read resources, poll approvals it
//!   queued itself. A read-only call never queues an approval, so a
//!   read-scope client never needs to poll one.
//! * `write` — everything `read` allows, plus state-changing tools
//!   (which still go through the gatekeeper/approval queue as usual).
//!
//! Which tools count as read-only is the same name heuristic the CLI
//! `hatch mcp call` command has always used (moved here so the server
//! enforces the same rule): `list`/`get`/`read`/`search`/`fetch`/
//! `echo`/`query` prefixes, or a name containing `list` / `read`.

/// One scope a client token may carry (shared with the product API's
/// per-client tokens; the definition lives in `hatch-core` so both
/// servers use the same one).
pub use hatch_core::client_tokens::Scope;

/// A tool name is treated as read-only when it only reads: list/get/
/// read/search/fetch/echo/query prefixes (or contains `list` / `read`).
/// Anything else is treated as possibly state-changing.
pub fn is_read_only_tool(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["list", "get", "read", "search", "fetch", "echo", "query"]
        .iter()
        .any(|p| n.starts_with(p))
        || n.contains("list")
        || n.contains("read")
}

/// Whether an action class counts as "read" for scope purposes. The
/// line is state change, not the name: `ReadPersonal` (own data),
/// `FetchWeb` (remote reads — including read-only connector ops, which
/// are classed `FetchWeb`; egress itself is still governed by the
/// gatekeeper's `FetchWeb` policy), and `BrowseLocal` (look at a page
/// without submitting anything). Everything else mutates state, sends
/// something out, or submits to a site, and needs the `write` scope.
pub fn is_read_class(class: hatch_policy::ActionClass) -> bool {
    use hatch_policy::ActionClass::*;
    matches!(class, ReadPersonal | FetchWeb | BrowseLocal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_heuristic() {
        for n in [
            "list_files",
            "get_user",
            "read_file",
            "search_web",
            "fetch_url",
            "echo",
            "query_db",
            "my_list_tool",
            "read_only",
        ] {
            assert!(is_read_only_tool(n), "{n}");
        }
        for n in [
            "create_file",
            "delete_user",
            "send_message",
            "write",
            "update_record",
            "run_command",
            "submit",
            "approval_status",
        ] {
            assert!(!is_read_only_tool(n), "{n}");
        }
    }

    #[test]
    fn scope_parse_and_read_classes() {
        use hatch_policy::ActionClass::*;
        assert_eq!(Scope::parse("read"), Ok(Scope::Read));
        assert_eq!(Scope::parse("WRITE"), Ok(Scope::Write));
        assert!(Scope::parse("admin").is_err());
        // Read classes: own data, remote reads, local browsing.
        for c in [ReadPersonal, FetchWeb, BrowseLocal] {
            assert!(is_read_class(c), "{c:?}");
        }
        // Everything else mutates state or sends something out.
        for c in [
            WriteMemory,
            DraftMessage,
            SendMessage,
            WriteCalendar,
            DeleteData,
            Payment,
            WriteFile,
            ExecCommand,
            BrowserSubmit,
        ] {
            assert!(!is_read_class(c), "{c:?}");
        }
    }
}
