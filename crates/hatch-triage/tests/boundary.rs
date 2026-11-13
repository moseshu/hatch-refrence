//! Integration test for the Phase 7 classification boundary.
//!
//! Each mail is fed to the real `classify` path with a scripted model that
//! returns a *wrong* category on purpose; the test asserts the deterministic
//! post-processing (`boundary.rs`) corrects it. The six required boundary
//! cases are covered, plus two extra guards (system notice, mislabelled
//! task).

use hatch_connectors::StoredMail;
use hatch_core::desensitize::AddressMapper;
use hatch_llm::scripted::ScriptedModel;
use hatch_llm::{ChatResponse, Router};
use hatch_triage::classify::classify;
use hatch_triage::Category;
use std::sync::Arc;

fn mail(from_addr: &str, from_name: &str, subject: &str, body: &str) -> StoredMail {
    StoredMail {
        id: "m1".into(),
        account: "me".into(),
        message_id: "<m1@x>".into(),
        uid: Some(1),
        from_addr: from_addr.into(),
        from_name: from_name.into(),
        to: vec![],
        subject: subject.into(),
        sent_at: None,
        body: body.into(),
    }
}

/// One scripted model answer per mail, in order.
fn router_for(model_categories: &[&str]) -> Router {
    let responses: Vec<ChatResponse> = model_categories
        .iter()
        .map(|c| {
            ChatResponse::text(format!(
                "{{\"category\":\"{c}\",\"priority\":\"normal\",\"summary\":\"摘要\",\"needs_reply\":{},\"reply_hint\":\"\",\"events\":[],\"suspicious\":false}}",
                matches!(*c, "reply" | "urgent")
            ))
        })
        .collect();
    Router::single(Arc::new(ScriptedModel::ok("m", responses)))
}

fn classify_all(
    mails: &[StoredMail],
    model_categories: &[&str],
) -> Vec<hatch_triage::TriageResult> {
    let router = router_for(model_categories);
    let now = hatch_core::time::now();
    let tz = chrono_tz::Asia::Shanghai;
    let mapper = AddressMapper::build(
        "me@126.example",
        "Moses",
        &mails
            .iter()
            .map(|m| (m.from_addr.clone(), m.from_name.clone()))
            .collect::<Vec<_>>(),
    );
    mails
        .iter()
        .map(|m| classify(&router, m, None, &now, &tz, &mapper).unwrap())
        .collect()
}

#[test]
fn boundary_cases_are_corrected_end_to_end() {
    let mails = vec![
        // 1. Pure FYI — model says action.
        mail(
            "zhang.wei@corp.example",
            "张伟",
            "周报模板更新",
            "各位，周报模板增加了“风险与依赖”一栏，下周起使用新模板。无需回复。",
        ),
        // 2. FYI carrying a question — model says fyi.
        mail(
            "li.lei@corp.example",
            "李雷",
            "项目进度同步",
            "项目进度同步如下：迁移已完成，压测通过。顺便问一下，试点能否 12 月第一周开始？",
        ),
        // 3. Reminder with a deadline — model says action.
        mail(
            "hr@corp.example",
            "人力资源部",
            "年假余额提醒",
            "您今年剩余年假 5 天，请在 12 月 31 日前安排使用。",
        ),
        // 4. Subscription notice — model says action.
        mail(
            "promo@shop.example",
            "某商城",
            "双 11 预售开启",
            "双 11 预售开启，满 300 减 50，限时抢购！退订请点这里。",
        ),
        // 5. Meeting invite, no reply needed — model says action.
        mail(
            "boss@corp.example",
            "王总",
            "周五下午全员大会",
            "周五下午 3 点全员大会已定在 5 楼会议室，请准时参加，无需回复。",
        ),
        // 6. Meeting invite needing a decision — model says fyi.
        mail(
            "li.lei@corp.example",
            "李雷",
            "周三下午评审时间",
            "架构评审想约在 11 月 4 日（周三）下午 15:00-16:00，地点 5 楼，方便吗？",
        ),
        // 7. System notice (GitHub review request) — model says action.
        mail(
            "noreply@github.example",
            "GitHub",
            "[hatch] Review requested on PR #42",
            "zhang-wei requested your review on #42: refactor gatekeeper rules.",
        ),
        // 8. A concrete task mislabelled fyi — model says fyi.
        mail(
            "boss@corp.example",
            "王总",
            "请提交 Q3 报销单",
            "请提交 Q3 报销单，本周五截止。",
        ),
    ];
    let model_said = [
        "action", "fyi", "action", "action", "action", "fyi", "action", "fyi",
    ];
    let got = classify_all(&mails, &model_said);
    let expected = [
        (Category::Fyi, false),
        (Category::Reply, true),
        (Category::Notification, false),
        (Category::Newsletter, false),
        (Category::Fyi, false),
        (Category::Reply, true),
        (Category::Notification, false),
        (Category::Action, false),
    ];
    for (i, ((cat, reply), r)) in expected.iter().zip(got.iter()).enumerate() {
        assert_eq!(
            r.category, *cat,
            "mail {i} (model said {}): {:?}",
            model_said[i], r.notes
        );
        assert_eq!(r.needs_reply, *reply, "mail {i} needs_reply");
    }
    // Every correction is visible in the receipt notes.
    assert!(got[0].notes.iter().any(|n| n.contains("边界规则")));
    assert!(got[1].notes.iter().any(|n| n.contains("R5")));
    assert!(got[2].notes.iter().any(|n| n.contains("R4")));
}
