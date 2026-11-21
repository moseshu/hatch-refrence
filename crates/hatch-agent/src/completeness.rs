//! Turn-end completeness self-check.
//!
//! The verification gate checks that claimed effects have evidence; it
//! never checks *coverage* — whether every sub-task in the user's
//! request was actually done ("copy A and B" → only A copied). This
//! module supplies the two pieces the turn loop needs to close that
//! gap generically:
//!
//! * [`is_side_effecting`] — the trigger classification: a turn that
//!   only read things has nothing to cover, so the check never fires
//!   for pure lookups or plain Q&A.
//! * [`completeness_note`] — the injected self-check message: the
//!   user's original request next to the list of actions already
//!   executed this turn, asking the model to verify coverage once
//!   before finishing. Recognition against an explicit list is an
//!   easier task than remembering while generating, so even the same
//!   (weak) model catches omissions it made minutes earlier.
//!
//! Known boundary: MCP tools carry class `FetchWeb`, so a turn whose
//! only effects went through MCP does not trigger the check. The
//! class taxonomy is shared with policy/taint decisions and is not
//! changed here.

use hatch_policy::ActionClass;

/// Cap on recorded actions per turn; beyond this the summary stops
/// growing (the check still fires, the list is just truncated).
pub const MAX_RECORDED_EFFECTS: usize = 32;

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
        s.push_str("（执行失败）");
    }
    s
}

/// The self-check message injected (as a system-trust user message)
/// when the model first tries to end a side-effecting turn.
pub fn completeness_note(user_input: &str, effects: &[String]) -> String {
    let mut note = String::from(
        "（系统自查：你正准备结束本轮，但请先对照用户本轮的原始要求，逐项核对本轮已经执行的动作是否有遗漏。\n",
    );
    note.push_str(&format!("用户本轮要求：「{}」\n", clip_chars(user_input, 1500)));
    note.push_str("本轮已执行的动作：\n");
    for e in effects {
        note.push_str(&format!("- {e}\n"));
    }
    note.push_str(
        "请逐项确认：要求中的每个文件、对象、参数和子任务是否都已完整执行（标记失败的动作不算完成）。\n\
         若有遗漏，立即补上缺失的调用，不要只口头说明；若确认无遗漏，直接给出最终答复。）",
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
        assert!(note.contains("cp({\"source\":\"b.txt\"})（执行失败）"));
        assert!(note.contains("立即补上缺失的调用"));
    }

    #[test]
    fn long_input_is_clipped() {
        let long = "x".repeat(5000);
        let note = completeness_note(&long, &[]);
        assert!(note.chars().count() < 2000);
        assert!(note.contains('…'));
    }
}
