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

const MAX_NOTE_CHARS: usize = 2400;

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
    const OMITTED_EFFECTS: &str = "- More actions in tool history.\n";
    const INSTRUCTIONS: &str = "Verify each requested outcome using the tool history. Lookups prepare deliverables. Answer in chat if that's all the user asked for. An approval proposal is a draft. Failed actions are unfinished.\n\
        If work is missing, make the missing calls now when you have the facts and authority. Stay within scope and follow normal approvals. Ask for essential missing facts or report blockers. Avoid repeating completed work. If all outcomes are covered, give the final answer.";

    let request = clip_chars(user_input, 1500);
    let mut note = format!("(System self-check)\nUser request: \"{request}\"\nRecorded actions:\n");
    if effects.is_empty() {
        note.push_str("- No action recorded. Refer to lookup results.\n");
    }
    let mut remaining = MAX_NOTE_CHARS - note.chars().count() - INSTRUCTIONS.chars().count();
    for (index, effect) in effects.iter().enumerate() {
        let line_chars = effect.chars().count() + 3;
        // Keep room to point to the full tool history whenever another action follows.
        let marker_chars = if index + 1 < effects.len() {
            OMITTED_EFFECTS.chars().count()
        } else {
            0
        };
        if line_chars + marker_chars > remaining {
            note.push_str(OMITTED_EFFECTS);
            break;
        }
        note.push_str(&format!("- {effect}\n"));
        remaining -= line_chars;
    }
    note.push_str(INSTRUCTIONS);
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
        let long = "字".repeat(5000);
        let empty_note = completeness_note(&long, &[]);
        assert!(empty_note.chars().count() < 2200);
        assert!(empty_note.contains(&format!("{}…", "字".repeat(1500))));

        let effect = effect_summary("write_file", &long, true);
        let effects = vec![effect.clone(); MAX_RECORDED_EFFECTS];
        let full_note = completeness_note(&long, &effects);
        assert!(full_note.chars().count() <= 2400);
        assert!(full_note.contains(effect.as_str()));
        assert!(full_note.contains("More actions in tool history"));
        assert!(full_note.contains("Avoid repeating completed work"));
    }
}
