//! Turn-end completeness self-check.
//!
//! The verification gate checks that claimed effects have evidence; it
//! never checks *coverage* — whether every sub-task in the user's
//! request was actually done ("copy A and B" → only A copied). This
//! module supplies the two pieces the turn loop needs to close that
//! gap generically:
//!
//! * [`is_side_effecting`] — distinguishes actions from lookups.
//!   An action or a multi-step lookup can precede an incomplete finish.
//! * [`completeness_note`] — the injected self-check message: the
//!   user's original request next to the list of actions already
//!   executed this turn, asking the model to verify coverage once
//!   before finishing. Recognition against an explicit list is an
//!   easier task than remembering while generating, so even the same
//!   (weak) model catches omissions it made minutes earlier.
//!
//! MCP tools carry class `FetchWeb`, so their results remain in the
//! transcript and aren't included in the action list below.

use hatch_policy::ActionClass;

/// Cap on recorded actions per turn; beyond this the summary stops
/// growing (the check still fires, the list is just truncated).
pub const MAX_RECORDED_EFFECTS: usize = 32;

/// Review a finish after multiple lookup steps, even with no recorded actions.
/// This keeps ordinary single-step lookups on their normal path.
pub const MIN_LOOKUP_STEPS: usize = 2;

/// Whether a tool class changes state in the world (as opposed to
/// only reading). Mirrors the read-only set the streak tracker uses.
pub fn is_side_effecting(class: ActionClass) -> bool {
    !matches!(
        class,
        ActionClass::ReadPersonal | ActionClass::FetchWeb | ActionClass::BrowseLocal
    )
}

/// One executed action, compactly rendered for the check list.
/// `failed` marks a dispatched call whose result reported an error —
/// attempted is not done, and the model must see the difference.
pub fn effect_summary(tool: &str, args_json: &str, failed: bool) -> String {
    let mut s = format!("{tool}({})", clip_chars(args_json, 160));
    if failed {
        s.push_str(" (execution failed)");
    }
    s
}

/// The self-check message injected (as a system-trust user message)
/// when the model first tries to end a turn after actions or multiple lookups.
pub fn completeness_note(user_input: &str, effects: &[String]) -> String {
    let mut note = String::from(
        "(System self-check: you are about to end this turn, but first compare against the user's original request for this turn and check item by item whether the actions executed this turn miss anything.\n",
    );
    note.push_str(&format!("User's request this turn: \"{}\"\n", clip_chars(user_input, 1500)));
    note.push_str("Actions executed this turn:\n");
    if effects.is_empty() {
        note.push_str("- No action recorded. Use the lookup results already in the conversation.\n");
    }
    for e in effects {
        note.push_str(&format!("- {e}\n"));
    }
    note.push_str(
        "Check each requested outcome against the evidence. Reading prepares a deliverable; an answer in chat is enough when the request asks for information or text in chat. An approval proposal counts as a submitted draft, and a failed action does not count as done.\n\
         If a requested outcome is still missing and you have enough information and authority, make the missing calls now. Use the normal approval path and preserve the user's scope. If an essential fact is missing or an action is blocked, ask for that fact or explain the blocker. Avoid repeating completed work. If nothing is missing, give the final answer directly.)",
    );
    note
}

/// Char-boundary-safe truncation with an ellipsis marker.
fn clip_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_classes_are_not_side_effecting() {
        for c in [
            ActionClass::ReadPersonal,
            ActionClass::FetchWeb,
            ActionClass::BrowseLocal,
        ] {
            assert!(!is_side_effecting(c), "{c:?}");
        }
        for c in [
            ActionClass::WriteMemory,
            ActionClass::DraftMessage,
            ActionClass::SendMessage,
            ActionClass::WriteCalendar,
            ActionClass::DeleteData,
            ActionClass::Payment,
            ActionClass::WriteFile,
            ActionClass::ExecCommand,
            ActionClass::BrowserSubmit,
        ] {
            assert!(is_side_effecting(c), "{c:?}");
        }
    }

    #[test]
    fn note_carries_request_and_effects_and_marks_failures() {
        let effects = vec![
            effect_summary("cp", "{\"source\":\"a.txt\"}", false),
            effect_summary("cp", "{\"source\":\"b.txt\"}", true),
        ];
        let note = completeness_note("把 a.txt 和 b.txt 都拷过去", &effects);
        assert!(note.contains("把 a.txt 和 b.txt 都拷过去"));
        assert!(note.contains("cp({\"source\":\"a.txt\"})"));
        assert!(note.contains("cp({\"source\":\"b.txt\"}) (execution failed)"));
        assert!(note.contains("make the missing calls now"));
    }

    #[test]
    fn long_input_is_clipped() {
        let long = "x".repeat(5000);
        let note = completeness_note(&long, &[]);
        assert!(note.chars().count() < 2200);
        assert!(note.contains('…'));
    }
}
