use super::*;
use std::sync::Arc;
use crate::seed::{self, SeedMail};
use hatch_core::time::parse_rfc3339;
use hatch_core::CancelToken;
use hatch_llm::scripted::ScriptedModel;
use hatch_llm::{ChatResponse, Router};
use hatch_policy::Priority;
use hatch_scheduler::{Engine, JobStore, NewJob, RunStatus};
use hatch_store::Db;
use hatch_triage::Category;

const SH: chrono_tz::Tz = chrono_tz::Asia::Shanghai;

fn ts(s: &str) -> hatch_core::time::Timestamp {
    parse_rfc3339(s).unwrap()
}

fn seeded() -> Db {
    let db = Db::open_in_memory().unwrap();
    let now = ts("2026-11-03T00:00:00Z");
    let m = SeedMail {
        message_id: "<m1@x>",
        from_addr: "wang@corp.example",
        from_name: "王经理",
        subject: "Q4 预算请今天确认",
        category: Category::Urgent,
        priority: Priority::Urgent,
        summary: "财务 18:00 前要最终数字",
        needs_reply: true,
        suspicious: false,
    };
    seed::mail(&db, "me@x", &m, &now).unwrap();
    seed::event(&db, "e1", "架构评审", ts("2026-11-03T07:00:00Z"), 60, "5楼").unwrap();
    seed::event(&db, "e2", "供应商电话", ts("2026-11-03T07:30:00Z"), 30, "").unwrap();
    seed::approval(&db, "k1", "回复王经理：确认预算", &now).unwrap();
    db.conn()
        .execute(
            "INSERT INTO workflow_runs(run_id, workflow, started_at, finished_at, status) VALUES('r','mail_triage',?1,?1,'ok')",
            hatch_store::params![hatch_core::time::to_rfc3339(&now)],
        )
        .unwrap();
    db
}

fn reply() -> ChatResponse {
    ChatResponse::text(
        r#"{"sections":[{"title":"今天要处理","items":[
            {"text":"王经理要求今天确认 Q4 预算","refs":["F1"]},
            {"text":"下午三点有董事会","refs":[]},
            {"text":"王经理升职了","refs":["F99"]}]},
          {"title":"日程","items":[{"text":"15:00 架构评审与供应商电话冲突","refs":["F2","F3","F4"]}]}]}"#,
    )
}

fn engine<'a>(db: &'a Db, router: Option<crate::RouterFactory>) -> Engine<'a> {
    let w = BriefWorker { router, attention: AttentionPolicy { tz: SH, ..Default::default() }, user_name: "Moses".into(), mail_configured: true };
    Engine::new(db, SH).register(KIND, Arc::new(w))
}

#[test]
fn items_must_cite_facts_unmentioned_facts_are_listed_and_one_brief_per_day() {
    let db = seeded();
    let model = Arc::new(ScriptedModel::ok("m", vec![reply()]));
    let m2 = model.clone();
    let e = engine(&db, Some(Arc::new(move || Ok(Router::single(m2.clone())))));
    JobStore::new(&db).add(&NewJob::new("brief", KIND, "daily 08:30"), &ts("2026-11-03T00:00:00Z"), &SH).unwrap();
    let r = e.tick(&ts("2026-11-03T00:30:00Z"), &CancelToken::new()).unwrap();
    assert_eq!(r.runs[0].status, RunStatus::Ok, "{:?}", r.runs[0]);
    let b = BriefStore::new(&db).latest(Some("2026-11-03")).unwrap().unwrap();
    assert!(b.content.contains("王经理要求今天确认 Q4 预算 〔F1〕"));
    assert!(!b.content.contains("董事会") && !b.content.contains("升职"), "uncited items are dropped:\n{}", b.content);
    assert!(b.content.contains("因缺少依据丢弃 2"));
    // F5 (the pending approval) was not cited — it is listed by code.
    assert!(b.content.contains("其他事项") && b.content.contains("回复王经理：确认预算"), "{}", b.content);
    // The facts the model saw were wrapped as untrusted.
    assert!(model.requests()[0].messages[1].content.contains("external_content"));
    // Hand-off + notification.
    assert_eq!(crate::WorkerOutputs::new(&db).unread().unwrap().len(), 1);
    // Same day again: deduplicated, no second model call.
    let r = e.run_now("brief", &ts("2026-11-03T05:00:00Z"), &CancelToken::new()).unwrap();
    assert_eq!(r.status, RunStatus::Skipped);
    assert_eq!(BriefStore::new(&db).count_for("2026-11-03").unwrap(), 1);
    assert_eq!(db.count("usage_log").unwrap(), 1, "the brief's model call was recorded");
}

#[test]
fn nothing_to_report_means_no_model_call_and_gaps_are_stated() {
    let db = Db::open_in_memory().unwrap();
    let model = Arc::new(ScriptedModel::ok("m", vec![]));
    let m2 = model.clone();
    let e = engine(&db, Some(Arc::new(move || Ok(Router::single(m2.clone())))));
    JobStore::new(&db).add(&NewJob::new("brief", KIND, "daily 08:30"), &ts("2026-11-03T00:00:00Z"), &SH).unwrap();
    let r = e.run_now("brief", &ts("2026-11-03T00:30:00Z"), &CancelToken::new()).unwrap();
    assert_eq!(r.status, RunStatus::Ok);
    let b = BriefStore::new(&db).latest(None).unwrap().unwrap();
    assert!(b.content.contains("无新事项"));
    assert!(b.content.contains("还没有运行过分诊") && b.content.contains("还没有导入任何日程"), "{}", b.content);
    assert!(model.requests().is_empty());
}

#[test]
fn without_a_model_the_brief_lists_facts_verbatim() {
    let db = seeded();
    let e = engine(&db, None);
    JobStore::new(&db).add(&NewJob::new("brief", KIND, "daily 08:30"), &ts("2026-11-03T00:00:00Z"), &SH).unwrap();
    e.run_now("brief", &ts("2026-11-03T00:30:00Z"), &CancelToken::new()).unwrap();
    let b = BriefStore::new(&db).latest(None).unwrap().unwrap();
    assert!(b.content.contains("没有配置模型"));
    assert!(b.content.contains("「架构评审」与「供应商电话」时间重叠"));
}
