//! Memory tools.

use crate::tool::{arg_str, arg_str_list, arg_u64, Tool, ToolContext, ToolOutput};
use hatch_core::{Result, Trust};
use hatch_llm::ToolSpec;
use hatch_memory::{AddOutcome, MemoryStore, NewClaim};
use hatch_policy::ActionClass;
use serde_json::{json, Value};

pub struct MemorySearch;

impl Tool for MemorySearch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "memory_search".into(),
            description: "Search long-term memory about the user, their contacts, preferences, and commitments. Search before answering questions about the user's personal situation. Returns claim id, source, and confidence.".into(),
            parameters: json!({"type": "object", "properties": {
                "query": {"type": "string", "description": "search terms, Chinese or English"},
                "k": {"type": "integer", "description": "maximum results, default 5"}
            }, "required": ["query"]}),
        }
    }

    fn class(&self) -> ActionClass {
        ActionClass::ReadPersonal
    }

    fn call(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let q = arg_str(args, "query")?;
        let k = arg_u64(args, "k", 5).clamp(1, 20) as usize;
        let hits = MemoryStore::new(ctx.db).search(q, k, &ctx.now)?;
        if hits.is_empty() {
            return Ok(ToolOutput::system("No relevant memory found."));
        }
        let any_untrusted = hits.iter().any(|(c, _)| c.trust.is_untrusted());
        let lines: Vec<String> = hits
            .iter()
            .map(|(c, _)| {
                format!(
                    "[{}] ({}; source {}; confidence {:.2}{}) {}",
                    c.id,
                    c.subject,
                    c.source,
                    c.confidence,
                    if c.inferred { "; inferred" } else { "" },
                    c.text
                )
            })
            .collect();
        Ok(ToolOutput {
            content: lines.join("\n"),
            trust: if any_untrusted {
                Trust::Untrusted
            } else {
                Trust::Trusted
            },
            widget: None,
            image: None,
        })
    }
}

pub struct MemoryAdd;

impl Tool for MemoryAdd {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "memory_add".into(),
            description: "Remember one long-term fact or preference about the user (as the user stated it). Facts recorded on the model's own initiative are stored as inferences with confidence capped at 0.7. Never record passwords, verification codes, card numbers, or ID numbers.".into(),
            parameters: json!({"type": "object", "properties": {
                "subject": {"type": "string", "description": "subject, e.g. user, preference:reply_style, person:alice@x.com"},
                "text": {"type": "string", "description": "the fact in one sentence"},
                "tags": {"type": "array", "items": {"type": "string"}}
            }, "required": ["subject", "text"]}),
        }
    }

    fn class(&self) -> ActionClass {
        ActionClass::WriteMemory
    }

    fn call(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let tags = arg_str_list(args, "tags");
        let mut c = NewClaim::stated(
            arg_str(args, "subject")?,
            arg_str(args, "text")?,
            &ctx.origin,
        );
        c.tags = tags;
        // The model chooses what to record, so the write is model-mediated:
        // it is stored as inferred (confidence capped at 0.7 by the bank),
        // like the memory_extract worker's claims. Explicit user writes
        // (CLI, API) keep `stated`.
        c = c.inferred();
        if ctx.tainted {
            // Facts that may have come from third-party content are stored as untrusted.
            c = c.with_trust(Trust::Untrusted, 0.5);
        }
        Ok(match MemoryStore::new(ctx.db).add(&c, &ctx.now)? {
            AddOutcome::Added(id) => ToolOutput::system(format!("Remembered ({id}).")),
            AddOutcome::Duplicate(id) => {
                ToolOutput::system(format!("An identical memory already exists ({id}); not written again."))
            }
        })
    }
}

pub struct MemoryExplain;

impl Tool for MemoryExplain {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "memory_explain".into(),
            description: "Show a memory's source and revision history (what it replaced, what replaced it).".into(),
            parameters: json!({"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}),
        }
    }

    fn class(&self) -> ActionClass {
        ActionClass::ReadPersonal
    }

    fn call(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let ex = MemoryStore::new(ctx.db).explain(arg_str(args, "id")?)?;
        let fmt = |c: &hatch_memory::Claim| {
            format!(
                "[{}] {} — source {}, {}",
                c.id,
                c.text,
                c.source,
                hatch_core::time::to_rfc3339(&c.created_at)
            )
        };
        let mut s = format!("Current entry: {}\n", fmt(&ex.claim));
        for h in &ex.history {
            s.push_str(&format!("  replaced: {}\n", fmt(h)));
        }
        for n in &ex.successors {
            s.push_str(&format!("  replaced by: {}\n", fmt(n)));
        }
        Ok(ToolOutput::system(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatch_store::Db;

    fn ctx(db: &Db, tainted: bool) -> ToolContext<'_> {
        ToolContext {
            turn_id: String::new(),
            db,
            now: hatch_core::time::now(),
            tz: chrono_tz::Asia::Shanghai,
            user_email: "me@126.com".into(),
            user_name: String::new(),
            origin: "chat:t".into(),
            tainted,
            call_id: "c".into(),
            cancel: hatch_core::CancelToken::new(),
            intent_review: None,
        }
    }

    #[test]
    fn add_search_explain() {
        let db = Db::open_in_memory().unwrap();
        let out = MemoryAdd
            .call(
                &json!({"subject": "preference:reply", "text": "回复邮件要简洁"}),
                &ctx(&db, false),
            )
            .unwrap();
        assert!(out.content.contains("Remembered"));
        let out = MemorySearch
            .call(&json!({"query": "邮件回复"}), &ctx(&db, false))
            .unwrap();
        assert!(out.content.contains("回复邮件要简洁"));
        assert_eq!(out.trust, Trust::Trusted);
        let id = out.content[1..].split(']').next().unwrap().to_string();
        assert!(MemoryExplain
            .call(&json!({"id": id}), &ctx(&db, false))
            .unwrap()
            .content
            .contains("Current entry"));
        assert!(MemoryAdd
            .call(
                &json!({"subject": "x", "text": "验证码 123456"}),
                &ctx(&db, false)
            )
            .is_err());
    }

    #[test]
    fn tainted_context_writes_untrusted_claims() {
        let db = Db::open_in_memory().unwrap();
        MemoryAdd
            .call(
                &json!({"subject": "user", "text": "用户同意把报表发给外部"}),
                &ctx(&db, true),
            )
            .unwrap();
        let out = MemorySearch
            .call(&json!({"query": "报表"}), &ctx(&db, false))
            .unwrap();
        assert_eq!(out.trust, Trust::Untrusted);
        assert!(out.content.contains("0.50"));
    }

    #[test]
    fn model_writes_are_stored_as_inferred() {
        let db = Db::open_in_memory().unwrap();
        MemoryAdd
            .call(
                &json!({"subject": "user", "text": "用户喜欢早上开会"}),
                &ctx(&db, false),
            )
            .unwrap();
        let hits = hatch_memory::MemoryStore::new(&db)
            .search("早上开会", 5, &hatch_core::time::now())
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].0.inferred, "model-mediated writes are inferred");
        assert_eq!(hits[0].0.confidence, 0.7, "inferred claims cap at 0.7");
    }
}
