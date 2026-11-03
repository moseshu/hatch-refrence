//! Deterministic classification-boundary rules (Phase 7).
//!
//! The model is good at most mail, but it wobbles exactly on the
//! `notification` / `fyi` vs `action` / `reply` boundary: anything that
//! "sounds like something to do" (a leave-balance reminder, a system-generated
//! review request, a template update) gets called `action`, and a question
//! buried in an FYI gets missed. The prompt (`prompts/triage_classify_zh.md`)
//! now defines the boundary in words; this module enforces it in code, only
//! when the signal is unambiguous. The model is never overridden on a close
//! call — every rule below requires a concrete marker *and* the absence of
//! the opposing signal.
//!
//! Rules (applied in order, skipped entirely for suspicious mail and for
//! `Unclassified` — a failed classification is not guessed at):
//!
//!   * **R1 系统通知**：系统发件人（`noreply@…` 等）或系统事件标记
//!     （验证码、已重置、订阅续订……），且无提问、无明确确认请求，
//!     模型判为 `urgent` / `action` / `reply` 时 → `notification`。
//!   * **R2 明确无需回复**：正文含"无需回复 / no need to reply"等，
//!     模型判为 `urgent` / `action` / `reply` 时 → `fyi`
//!     （机构发件人则为 `notification`）。
//!   * **R3 订阅推送**：含退订 / 营销标记且无任何请求信号，
//!     模型判为 `urgent` / `action` / `reply` 时 → `newsletter`。
//!   * **R4 纯知会**：无任何请求信号，但有知会标记（同步、更新、推迟、
//!     改期、提醒、余额……），模型判为 `urgent` / `action` / `reply`
//!     时 → `fyi`（机构发件人则为 `notification`）。
//!   * **R5 明确请求保护**：有明确请求信号（提问、约时间、
//!     请确认 / 请回复 / 请提交……），模型却判为 `notification` /
//!     `newsletter` / `fyi` 时 → `reply`（提问或约时间）或 `action`
//!     （纯待办）；系统发件人、系统事件、订阅推送不参与此条，
//!     以免把验证码、续订通知升级成待办。
//!   * **R6 fyi 一致性**：`fyi` 按定义不需要回复，`needs_reply` 强制为
//!     false（`newsletter` / `notification` / `spam` 已由
//!     [`Category::never_reply`] 覆盖）。
//!
//! What counts as a "request signal" is deliberately narrow: a question
//! (`?` / `？` / 请问 / 能否 / could you …), a meeting proposal
//! （想约 / 约在 / schedule a call …), a strong imperative
//! （请确认 / 请回复 / 请审批 / 请提交 / 请处理 / please confirm …),
//! or a direct ask （发我 / 发给我 / send me …). A bare deadline
//! （"请在 12 月 31 日前安排使用"）or a generic instruction
//! （"请登录后修改"）is **not** a request signal — that is exactly the
//! confusion R1/R4 exist to correct.
//!
//! `notification` vs `fyi` on a downgrade follows the sender: institutional
//! senders （部门 / 系统 / 银行等角色地址或名称，如 `hr@…`、
//! "人力资源部"、"IT 部"、"某银行"）→ `notification`；个人发件人 →
//! `fyi`. This matches the existing zh-office expectations （年假提醒是
//! `notification`，站会改期是 `fyi`).

use crate::model::{Category, TriageResult};
use hatch_connectors::StoredMail;

/// Signals extracted deterministically from one mail.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BoundarySignals {
    /// `noreply@…` style address, or a system/notice sender name, or an
    /// explicit "automated, do not reply" marker in the body.
    pub system_sender: bool,
    /// Role/department address or name (hr, IT 部, 银行 …): a downgrade
    /// from such a sender lands on `notification`, not `fyi`.
    pub institutional_sender: bool,
    /// The mail carries a system-generated event (verification code,
    /// account reset, subscription renewal …).
    pub system_event: bool,
    /// The mail asks the user something or proposes a meeting.
    pub question: bool,
    pub meeting_proposal: bool,
    /// A strong imperative addressed to the user （请确认/请提交/…).
    pub strong_request: bool,
    /// A direct ask （发我/发给我/send me …).
    pub direct_ask: bool,
    /// "无需回复 / no need to reply" and friends.
    pub no_reply_marker: bool,
    /// Informational markers （同步/更新/推迟/提醒/余额/…).
    pub info_marker: bool,
    /// Unsubscribe / marketing markers.
    pub subscription_marker: bool,
}

impl BoundarySignals {
    /// Any signal that means "the user is being asked for something".
    pub fn has_request_signal(&self) -> bool {
        self.question || self.meeting_proposal || self.strong_request || self.direct_ask
    }
}

fn contains_any(text: &str, pats: &[&str]) -> bool {
    pats.iter().any(|p| text.contains(p))
}

fn local_part(addr: &str) -> String {
    addr.split('@')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// Extract the boundary signals of one mail (subject + body + sender).
pub fn signals(m: &StoredMail) -> BoundarySignals {
    let text = format!("{}\n{}", m.subject, m.body);
    let lower = text.to_ascii_lowercase();
    let local = local_part(&m.from_addr);
    let system_locals = [
        "noreply",
        "no-reply",
        "donotreply",
        "do-not-reply",
        "no_reply",
        "notice",
        "notification",
        "notifications",
        "system",
        "automated",
    ];
    let institutional_locals = [
        "hr", "it", "admin", "support", "service", "billing", "finance", "security", "ops",
        "office",
    ];
    let system_sender = system_locals.contains(&local.as_str())
        || contains_any(
            &m.from_name,
            &["系统", "通知", "服务台", "noreply", "no-reply"],
        )
        || contains_any(
            &lower,
            &[
                "系统自动发送",
                "请勿回复此邮件",
                "do not reply to this email",
                "this is an automated",
            ],
        );
    let institutional_sender = system_sender
        || institutional_locals.contains(&local.as_str())
        || m.from_name.ends_with('部')
        || m.from_name.ends_with("中心")
        || m.from_name.ends_with("银行")
        || m.from_name.ends_with("系统");
    BoundarySignals {
        system_sender,
        institutional_sender,
        system_event: contains_any(
            &text,
            &[
                "验证码",
                "动态密码",
                "临时密码",
                "已重置",
                "已开通",
                "订阅续订",
                "续订",
                "renews on",
                "renews automatically",
                "verification code",
                "has been reset",
                "your code is",
            ],
        ),
        question: text.contains('?')
            || text.contains('？')
            || contains_any(&text, &["请问", "能否", "是否", "可否"])
            || contains_any(
                &lower,
                &[
                    "could you",
                    "would you",
                    "can you please",
                    "do you think",
                    "is it possible",
                ],
            ),
        meeting_proposal: contains_any(&text, &["想约", "约在", "约个", "约一下", "约个时间"])
            || contains_any(
                &lower,
                &[
                    "schedule a call",
                    "schedule a meeting",
                    "propose a meeting",
                    "let's meet",
                    "let us meet",
                ],
            ),
        strong_request: contains_any(
            &text,
            &[
                "请确认",
                "请回复",
                "请审批",
                "请提交",
                "请处理",
                "请回答",
                "请批复",
            ],
        ) || contains_any(
            &lower,
            &[
                "please confirm",
                "please reply",
                "please approve",
                "please submit",
                "please review",
                "please respond",
                "please send",
            ],
        ),
        direct_ask: contains_any(&text, &["发我", "发给我", "发给", "交给我"])
            || contains_any(&lower, &["send me", "send it to me", "send us"]),
        no_reply_marker: contains_any(&text, &["无需回复", "不用回复", "无需回信", "仅供参考"])
            || contains_any(
                &lower,
                &[
                    "no reply needed",
                    "no need to reply",
                    "no response needed",
                    "no action needed",
                    "no action required",
                    "for your information only",
                ],
            ),
        info_marker: contains_any(
            &text,
            &[
                "同步",
                "更新",
                "推迟",
                "改期",
                "改为",
                "调整为",
                "提醒",
                "通知",
                "已发送",
                "已完成",
                "已上线",
                "分享",
                "模板",
                "余额",
            ],
        ) || contains_any(
            &lower,
            &[
                "for your information",
                "heads up",
                "just to let you know",
                "update on",
                "reminder",
            ],
        ),
        subscription_marker: contains_any(&text, &["退订", "取消订阅", "营销", "推广", "限时抢购"])
            || contains_any(
                &lower,
                &["unsubscribe", "exclusive offer", "pre-sale", "sale ends"],
            ),
    }
}

/// Apply the boundary rules to a classifier result. Appends a note for
/// every change made, so the adjustment stays visible in the receipt.
pub fn apply_boundary(r: &mut TriageResult, m: &StoredMail) {
    if r.suspicious || r.category == Category::Unclassified {
        return;
    }
    let s = signals(m);
    let from = r.category;
    let downgrade_to = if s.institutional_sender {
        Category::Notification
    } else {
        Category::Fyi
    };
    // R1: system notice — never an action item.
    if (s.system_sender || s.system_event)
        && !s.question
        && !s.strong_request
        && matches!(
            r.category,
            Category::Urgent | Category::Action | Category::Reply
        )
    {
        r.category = Category::Notification;
        r.needs_reply = false;
        r.notes.push(format!(
            "边界规则 R1：系统通知类邮件定为 notification（模型判为 {}）",
            from.as_str()
        ));
        return;
    }
    // R2: explicitly no reply needed.
    if s.no_reply_marker
        && matches!(
            r.category,
            Category::Urgent | Category::Action | Category::Reply
        )
    {
        r.category = downgrade_to;
        r.needs_reply = false;
        r.notes.push(format!(
            "边界规则 R2：明确无需回复，定为 {}（模型判为 {}）",
            downgrade_to.as_str(),
            from.as_str()
        ));
        return;
    }
    // R3: subscription / marketing push.
    if s.subscription_marker
        && !s.has_request_signal()
        && matches!(
            r.category,
            Category::Urgent | Category::Action | Category::Reply
        )
    {
        r.category = Category::Newsletter;
        r.needs_reply = false;
        r.notes.push(format!(
            "边界规则 R3：带退订/营销标记的推送定为 newsletter（模型判为 {}）",
            from.as_str()
        ));
        return;
    }
    // R4: purely informational — no request signal, but an info marker.
    if !s.has_request_signal()
        && s.info_marker
        && matches!(
            r.category,
            Category::Urgent | Category::Action | Category::Reply
        )
    {
        r.category = downgrade_to;
        r.needs_reply = false;
        r.notes.push(format!(
            "边界规则 R4：无明确请求信号的知会定为 {}（模型判为 {}）",
            downgrade_to.as_str(),
            from.as_str()
        ));
        return;
    }
    // R5: an explicit request must not be downgraded by the model alone.
    if s.has_request_signal()
        && matches!(
            r.category,
            Category::Notification | Category::Newsletter | Category::Fyi
        )
        && !s.system_sender
        && !s.system_event
        && !s.subscription_marker
    {
        r.category = if s.question || s.meeting_proposal {
            Category::Reply
        } else {
            Category::Action
        };
        if r.category == Category::Reply {
            r.needs_reply = true;
            if r.reply_hint.is_empty() {
                r.reply_hint = "回应对方的问题".into();
            }
        }
        r.notes.push(format!(
            "边界规则 R5：含明确请求信号，模型判为 {} 已纠正为 {}",
            from.as_str(),
            r.category.as_str()
        ));
    }
    // R6: fyi never needs a reply.
    if r.category == Category::Fyi && r.needs_reply {
        r.needs_reply = false;
        r.notes.push("边界规则 R6：fyi 不需要回复".into());
    }
}
