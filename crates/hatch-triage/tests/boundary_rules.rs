//! Unit tests for the Phase 7 boundary rules (one rule per test, both
//! directions). Uses only the public API of `hatch_triage::boundary`.

use hatch_connectors::StoredMail;
use hatch_policy::Priority;
use hatch_triage::boundary::apply_boundary;
use hatch_triage::{Category, TriageResult};

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

fn result(category: Category, needs_reply: bool) -> TriageResult {
    TriageResult {
        mail_id: "m1".into(),
        category,
        priority: Priority::Normal,
        summary: "摘要".into(),
        needs_reply,
        reply_hint: String::new(),
        events: vec![],
        suspicious: false,
        notes: vec![],
        model: "test".into(),
    }
}

fn adjusted(category: Category, needs_reply: bool, m: &StoredMail) -> TriageResult {
    let mut r = result(category, needs_reply);
    apply_boundary(&mut r, m);
    r
}

#[test]
fn r1_system_notice_is_not_action() {
    // GitHub review request from a noreply address (zh-office m109).
    let m = mail(
        "noreply@github.example",
        "GitHub",
        "[hatch] Review requested on PR #42",
        "zhang-wei requested your review on #42: refactor gatekeeper rules.",
    );
    let r = adjusted(Category::Action, false, &m);
    assert_eq!(r.category, Category::Notification);
    assert!(r.notes.iter().any(|n| n.contains("R1")));
    // VPN reset: system-event marker + generic instruction (m114).
    let m = mail(
        "it@corp.example",
        "IT 部",
        "VPN 账号已重置",
        "您的 VPN 账号已重置。临时密码：Abc12345!x 请首次登录后立即修改。",
    );
    let r = adjusted(Category::Action, false, &m);
    assert_eq!(r.category, Category::Notification);
    // But a system mail that really asks a question is left alone.
    let m = mail(
        "noreply@bank.example",
        "某银行",
        "请确认这笔交易是您本人操作吗？",
        "您的账户有一笔异常交易，请问是否为您本人操作？",
    );
    let r = adjusted(Category::Action, false, &m);
    assert_eq!(r.category, Category::Action, "带提问的系统邮件不强制降级");
}

#[test]
fn r2_explicit_no_reply_is_fyi() {
    let m = mail(
        "zhang.wei@corp.example",
        "张伟",
        "周报模板更新",
        "各位，周报模板增加了“风险与依赖”一栏，下周起使用新模板。无需回复。",
    );
    let r = adjusted(Category::Action, true, &m);
    assert_eq!(r.category, Category::Fyi);
    assert!(!r.needs_reply);
}

#[test]
fn r3_subscription_push_is_newsletter() {
    let m = mail(
        "promo@shop.example",
        "某商城",
        "双 11 预售开启",
        "双 11 预售开启，满 300 减 50，限时抢购！退订请点这里。",
    );
    let r = adjusted(Category::Action, false, &m);
    assert_eq!(r.category, Category::Newsletter);
}

#[test]
fn r4_pure_info_without_request_signal() {
    // Leave-balance reminder with a deadline, but no concrete ask (m108).
    let m = mail(
        "hr@corp.example",
        "人力资源部",
        "年假余额提醒",
        "您今年剩余年假 5 天，请在 12 月 31 日前安排使用。",
    );
    let r = adjusted(Category::Action, false, &m);
    assert_eq!(
        r.category,
        Category::Notification,
        "机构发件人的知会 → notification"
    );
    // Schedule change from a person (m113).
    let m = mail(
        "boss@corp.example",
        "王总",
        "明早站会改为 9:30",
        "明天早上站会推迟到 9:30，地点不变。",
    );
    let r = adjusted(Category::Action, false, &m);
    assert_eq!(r.category, Category::Fyi);
    // A bare deadline *with* a concrete ask is not downgraded.
    let m = mail(
        "boss@corp.example",
        "王总",
        "Q4 预算请周五前发我",
        "Q4 预算请在周五前发我。",
    );
    let r = adjusted(Category::Action, false, &m);
    assert_eq!(r.category, Category::Action);
}

#[test]
fn r5_explicit_request_is_not_downgraded() {
    // FYI carrying a question (m111 shape).
    let m = mail(
        "alice@client.example",
        "Alice Smith",
        "Proposal follow-up",
        "Hi Moses,\nCould you confirm whether the pilot can start in the first week of December?",
    );
    let r = adjusted(Category::Fyi, false, &m);
    assert_eq!(r.category, Category::Reply);
    assert!(r.needs_reply);
    // A task mislabelled fyi.
    let m = mail(
        "boss@corp.example",
        "王总",
        "请提交 Q3 报销单",
        "请提交 Q3 报销单，本周五截止。",
    );
    let r = adjusted(Category::Fyi, false, &m);
    assert_eq!(r.category, Category::Action);
    assert!(!r.needs_reply, "action 不等于需要回信");
    // A meeting proposal mislabelled fyi.
    let m = mail(
        "li.lei@corp.example",
        "李雷",
        "周三下午评审时间",
        "架构评审想约在 11 月 4 日（周三）下午 15:00-16:00，地点 5 楼，方便吗？",
    );
    let r = adjusted(Category::Notification, false, &m);
    assert_eq!(r.category, Category::Reply);
    // But a system notice with a question mark stays a notice.
    let m = mail(
        "noreply@bank.example",
        "某银行",
        "动态验证码",
        "您的验证码是 483920，用于网上银行登录，5 分钟内有效。请勿泄露给他人。",
    );
    let r = adjusted(Category::Notification, false, &m);
    assert_eq!(r.category, Category::Notification);
}

#[test]
fn r6_fyi_never_needs_reply() {
    let m = mail(
        "a@corp.example",
        "张伟",
        "进度同步",
        "本周进度同步如下：迁移已完成。",
    );
    let r = adjusted(Category::Fyi, true, &m);
    assert_eq!(r.category, Category::Fyi);
    assert!(!r.needs_reply);
}

#[test]
fn boundary_skips_suspicious_and_unclassified() {
    let m = mail(
        "noreply@github.example",
        "GitHub",
        "[hatch] Review requested",
        "review requested",
    );
    let mut r = result(Category::Spam, false);
    r.suspicious = true;
    apply_boundary(&mut r, &m);
    assert_eq!(r.category, Category::Spam, "可疑邮件不由边界规则改动");
    let r = adjusted(Category::Unclassified, false, &m);
    assert_eq!(r.category, Category::Unclassified);
}

#[test]
fn real_action_mail_is_untouched() {
    let m = mail(
        "boss@corp.example",
        "王总",
        "Q4 预算请今天下班前确认",
        "请确认研发线的两项调整是否同意，有问题直接回我。",
    );
    let r = adjusted(Category::Urgent, true, &m);
    assert_eq!(r.category, Category::Urgent);
    assert!(r.needs_reply);
    assert!(r.notes.is_empty(), "无边界调整时不写备注");
}
