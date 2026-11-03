//! Action taxonomy.

use hatch_core::{Error, Result};
use serde::{Deserialize, Serialize};

/// What kind of effect an action has on the world.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ActionClass {
    /// Read the user's own data (mail, calendar, memory).
    ReadPersonal,
    /// Write to the assistant's memory.
    WriteMemory,
    /// Create a local draft (nothing leaves the machine).
    DraftMessage,
    /// Send anything to another person or service.
    SendMessage,
    /// Create / move / cancel calendar events.
    WriteCalendar,
    /// Delete user data.
    DeleteData,
    /// Spend money.
    Payment,
    /// Create or modify a file inside the workspace (checkpointed, so reversible).
    WriteFile,
    /// Run a command in the sandbox.
    ExecCommand,
    /// Fetch a web page (the URL itself leaves the machine).
    FetchWeb,
}

impl ActionClass {
    pub const ALL: [ActionClass; 10] = [
        ActionClass::ReadPersonal,
        ActionClass::WriteMemory,
        ActionClass::DraftMessage,
        ActionClass::SendMessage,
        ActionClass::WriteCalendar,
        ActionClass::DeleteData,
        ActionClass::Payment,
        ActionClass::WriteFile,
        ActionClass::ExecCommand,
        ActionClass::FetchWeb,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            ActionClass::ReadPersonal => "read_personal",
            ActionClass::WriteMemory => "write_memory",
            ActionClass::DraftMessage => "draft_message",
            ActionClass::SendMessage => "send_message",
            ActionClass::WriteCalendar => "write_calendar",
            ActionClass::DeleteData => "delete_data",
            ActionClass::Payment => "payment",
            ActionClass::WriteFile => "write_file",
            ActionClass::ExecCommand => "exec_command",
            ActionClass::FetchWeb => "fetch_web",
        }
    }

    pub fn parse(s: &str) -> Result<ActionClass> {
        Self::ALL
            .into_iter()
            .find(|c| c.as_str() == s)
            .ok_or_else(|| Error::Invalid(format!("unknown action class {s}")))
    }

    pub fn reversibility(&self) -> Reversibility {
        match self {
            ActionClass::ReadPersonal
            | ActionClass::WriteMemory
            | ActionClass::DraftMessage
            | ActionClass::WriteFile
            | ActionClass::FetchWeb => Reversibility::Reversible,
            ActionClass::WriteCalendar => Reversibility::Compensable,
            ActionClass::SendMessage | ActionClass::DeleteData | ActionClass::Payment | ActionClass::ExecCommand => {
                Reversibility::Irreversible
            }
        }
    }

    /// Does the effect leave the user's own data boundary?
    pub fn is_external(&self) -> bool {
        matches!(self, ActionClass::SendMessage | ActionClass::WriteCalendar | ActionClass::Payment | ActionClass::DeleteData)
    }

    /// Classes that must not act on their own while the context holds
    /// untrusted content (the taint rule).
    pub fn taint_gated(&self) -> bool {
        self.is_external() || matches!(self, ActionClass::WriteFile | ActionClass::ExecCommand | ActionClass::FetchWeb)
    }

    /// Most autonomy the user may ever configure for this class without a grant.
    pub fn autonomy_ceiling(&self) -> Autonomy {
        match self {
            ActionClass::Payment => Autonomy::Suggest,
            c if c.reversibility() == Reversibility::Irreversible => Autonomy::Draft,
            _ => Autonomy::Silent,
        }
    }

    pub fn default_autonomy(&self) -> Autonomy {
        match self {
            ActionClass::ReadPersonal => Autonomy::Silent,
            ActionClass::WriteMemory | ActionClass::DraftMessage => Autonomy::ActNotify,
            ActionClass::SendMessage | ActionClass::WriteCalendar | ActionClass::DeleteData => Autonomy::Draft,
            ActionClass::WriteFile | ActionClass::ExecCommand => Autonomy::Draft,
            ActionClass::FetchWeb => Autonomy::Silent,
            ActionClass::Payment => Autonomy::Suggest,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reversibility {
    /// Can be undone completely (local drafts, memory).
    Reversible,
    /// Can be compensated (cancel the event you created).
    Compensable,
    /// Cannot be taken back (sent mail, payments, deletions).
    Irreversible,
}

/// How much the assistant may do on its own. L0..L3.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Autonomy {
    /// L0 — may only suggest; never executes.
    Suggest = 0,
    /// L1 — prepares the exact action; executes after explicit approval.
    Draft = 1,
    /// L2 — executes, then tells the user.
    ActNotify = 2,
    /// L3 — executes silently.
    Silent = 3,
}

impl Autonomy {
    pub fn parse(s: &str) -> Result<Autonomy> {
        match s {
            "suggest" | "l0" | "L0" => Ok(Autonomy::Suggest),
            "draft" | "l1" | "L1" => Ok(Autonomy::Draft),
            "act_notify" | "l2" | "L2" => Ok(Autonomy::ActNotify),
            "silent" | "l3" | "L3" => Ok(Autonomy::Silent),
            _ => Err(Error::Invalid(format!("unknown autonomy level {s}"))),
        }
    }
}

/// Sensitivity labels attached to the data an action would carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataLabel {
    /// Details of the user's calendar beyond free/busy.
    CalendarDetails,
    /// Content of messages written by third parties.
    OtherPeoplesMessages,
    /// Private facts from memory.
    MemoryPrivate,
    /// Credentials or sensitive numbers — never allowed to leave.
    Secret,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taxonomy() {
        for c in ActionClass::ALL {
            assert_eq!(ActionClass::parse(c.as_str()).unwrap(), c);
            assert!(c.default_autonomy() <= c.autonomy_ceiling());
        }
        assert_eq!(ActionClass::SendMessage.reversibility(), Reversibility::Irreversible);
        assert_eq!(ActionClass::Payment.autonomy_ceiling(), Autonomy::Suggest);
        assert_eq!(Autonomy::parse("L2").unwrap(), Autonomy::ActNotify);
        assert!(ActionClass::parse("launch_missiles").is_err());
        assert_eq!(ActionClass::ExecCommand.autonomy_ceiling(), Autonomy::Draft);
        assert_eq!(ActionClass::WriteFile.reversibility(), Reversibility::Reversible);
        assert!(ActionClass::FetchWeb.taint_gated() && !ActionClass::FetchWeb.is_external());
    }
}
