//! Calendar read tools.

use crate::tool::{arg_opt_str, arg_u64, Tool, ToolContext, ToolOutput};
use chrono::Duration;
use hatch_connectors::CalendarRepo;
use hatch_core::time::fmt_local;
use hatch_core::Result;
use hatch_llm::ToolSpec;
use hatch_policy::ActionClass;
use serde_json::{json, Value};

pub struct CalendarList;

impl Tool for CalendarList {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "calendar_list".into(),
            description: "列出用户日程。可指定开始时间（默认现在）与天数（默认 3）。".into(),
            parameters: json!({"type": "object", "properties": {
                "from": {"type": "string", "description": "RFC3339 或 'YYYY-MM-DD HH:MM'，默认现在"},
                "days": {"type": "integer"}
            }}),
        }
    }

    fn class(&self) -> ActionClass {
        ActionClass::ReadPersonal
    }

    fn call(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let from = match arg_opt_str(args, "from") {
            Some(s) => super::parse_user_time(s, &ctx.tz)?,
            None => ctx.now,
        };
        let days = arg_u64(args, "days", 3).clamp(1, 31) as i64;
        let evs = CalendarRepo::new(ctx.db).between(&from, &(from + Duration::days(days)))?;
        if evs.is_empty() {
            return Ok(ToolOutput::system(format!("{} 起 {days} 天内没有日程。", fmt_local(&from, &ctx.tz))));
        }
        Ok(ToolOutput::system(
            evs.iter()
                .map(|e| format!("- {} — {} {}{}", fmt_local(&e.starts_at, &ctx.tz), fmt_local(&e.ends_at, &ctx.tz), e.title,
                    if e.location.is_empty() { String::new() } else { format!(" @{}", e.location) }))
                .collect::<Vec<_>>()
                .join("\n"),
        ))
    }
}

pub struct CalendarFreeSlots;

impl Tool for CalendarFreeSlots {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "calendar_free_slots".into(),
            description: "在工作时间（9:00-18:00）内查找空闲时段。".into(),
            parameters: json!({"type": "object", "properties": {
                "days": {"type": "integer", "description": "向后查找的天数，默认 5"},
                "duration_minutes": {"type": "integer", "description": "时长，默认 60"},
                "limit": {"type": "integer"}
            }}),
        }
    }

    fn class(&self) -> ActionClass {
        ActionClass::ReadPersonal
    }

    fn call(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let days = arg_u64(args, "days", 5).clamp(1, 30) as i64;
        let mins = arg_u64(args, "duration_minutes", 60).clamp(15, 480) as i64;
        let limit = arg_u64(args, "limit", 5).clamp(1, 20) as usize;
        let slots = CalendarRepo::new(ctx.db).free_slots(&ctx.now, &(ctx.now + Duration::days(days)), Duration::minutes(mins), (9, 18), &ctx.tz, limit)?;
        if slots.is_empty() {
            return Ok(ToolOutput::system("指定范围内没有合适的空闲时段。"));
        }
        Ok(ToolOutput::system(
            slots.iter().map(|(s, e)| format!("- {} — {}", fmt_local(s, &ctx.tz), fmt_local(e, &ctx.tz))).collect::<Vec<_>>().join("\n"),
        ))
    }
}
