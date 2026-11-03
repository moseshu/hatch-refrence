//! What kind of page is this, and what would a click do?
//!
//! Both answers come from code, from element attributes and page features —
//! never from the model. When unsure, the more cautious answer wins: an
//! unknown click is treated as a submission.

use crate::snapshot::{Element, Snapshot};
use serde::{Deserialize, Serialize};

/// Condition of a loaded page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum PageCondition {
    Ok,
    LoginRequired(String),
    Captcha(String),
    Blocked(String),
}

const CAPTCHA_WORDS: &[&str] = &[
    "captcha", "recaptcha", "hcaptcha", "人机验证", "安全验证", "验证您是人类", "验证你是人类", "我不是机器人", "i'm not a robot",
    "verify you are human", "滑动验证", "拼图验证", "请完成验证",
];
const LOGIN_WORDS: &[&str] = &["登录", "登入", "log in", "login", "sign in", "signin"];
const BLOCK_WORDS: &[&str] = &[
    "access denied", "unusual traffic", "are you a robot", "automated access", "request blocked", "访问被拒绝", "请求过于频繁", "访问受限", "禁止访问",
];
const PAY_WORDS: &[&str] = &[
    "支付", "付款", "购买", "下单", "结算", "确认订单", "pay", "buy", "purchase", "place order", "checkout", "check out", "subscribe", "订阅",
];
const PAY_URL_PARTS: &[&str] = &["checkout", "/payment", "/pay", "billing", "/cart", "/order/confirm"];
const SUBMIT_WORDS: &[&str] = &[
    "提交", "发送", "确认", "确定", "保存", "删除", "注销", "申请", "预订", "注册", "submit", "send", "confirm", "save", "delete", "remove", "apply",
    "book now", "register", "发布",
];

fn has_any(hay: &str, words: &[&str]) -> Option<String> {
    let h = hay.to_lowercase();
    words.iter().find(|w| h.contains(&w.to_lowercase())).map(|w| w.to_string())
}

/// Classify a loaded page from its snapshot and the main document's HTTP status.
pub fn classify_page(s: &Snapshot, status: Option<u16>) -> PageCondition {
    let head: String = s.text.chars().take(4000).collect();
    let probe = format!("{} {}", s.title, head);
    if s.markers.captcha {
        return PageCondition::Captcha("页面包含验证码组件".into());
    }
    if let Some(w) = has_any(&probe, CAPTCHA_WORDS) {
        return PageCondition::Captcha(format!("页面要求人机验证（“{w}”）"));
    }
    match status {
        Some(401) | Some(407) => return PageCondition::LoginRequired(format!("HTTP {}", status.unwrap_or(0))),
        Some(403) | Some(429) => return PageCondition::Blocked(format!("HTTP {}", status.unwrap_or(0))),
        _ => {}
    }
    if s.markers.password {
        if let Some(w) = has_any(&probe, LOGIN_WORDS) {
            return PageCondition::LoginRequired(format!("登录页（密码框 + “{w}”）"));
        }
    }
    if s.elements.len() < 8 {
        if let Some(w) = has_any(&probe, BLOCK_WORDS) {
            return PageCondition::Blocked(format!("“{w}”"));
        }
    }
    PageCondition::Ok
}

/// Is this a checkout / payment page?
pub fn is_checkout(s: &Snapshot) -> bool {
    let url = s.url.to_lowercase();
    s.markers.card || PAY_URL_PARTS.iter().any(|p| url.contains(p))
}

/// What a click on an element would do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClickKind {
    /// Follows a link (loads another page).
    Navigate,
    /// Changes only the page itself (tab, disclosure, checkbox).
    Local,
    /// Submits something to the site.
    Submit,
    /// Pays or buys.
    Payment,
}

pub fn classify_click(e: &Element, page: &Snapshot) -> ClickKind {
    let words = format!("{} {} {} {}", e.text, e.label, e.name, e.id);
    if has_any(&words, PAY_WORDS).is_some() || (is_checkout(page) && (e.in_form || e.tag == "button" || e.r#type == "submit")) {
        return ClickKind::Payment;
    }
    let href = e.href.trim().to_lowercase();
    let real_link = e.tag == "a" && !href.is_empty() && !href.starts_with('#') && !href.starts_with("javascript:");
    if real_link && e.role != "button" && has_any(&words, SUBMIT_WORDS).is_none() {
        return ClickKind::Navigate;
    }
    let local_widget = e.tag == "summary"
        || matches!(e.role.as_str(), "tab" | "checkbox")
        || matches!(e.r#type.as_str(), "checkbox" | "radio")
        || (e.expanded && !e.in_form);
    if local_widget && has_any(&words, SUBMIT_WORDS).is_none() {
        return ClickKind::Local;
    }
    ClickKind::Submit
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::Markers;

    fn el(tag: &str, ty: &str, text: &str) -> Element {
        Element { reference: 1, tag: tag.into(), r#type: ty.into(), text: text.into(), ..Default::default() }
    }

    fn page(url: &str, text: &str, markers: Markers) -> Snapshot {
        Snapshot { url: url.into(), title: String::new(), text: text.into(), elements: vec![], markers }
    }

    #[test]
    fn page_conditions() {
        let m = Markers::default();
        assert_eq!(classify_page(&page("http://x/a", "普通文章", m.clone()), Some(200)), PageCondition::Ok);
        assert!(matches!(classify_page(&page("http://x/v", "请完成人机验证", m.clone()), Some(200)), PageCondition::Captcha(_)));
        assert!(matches!(
            classify_page(&page("http://x/v", "x", Markers { captcha: true, ..Default::default() }), Some(200)),
            PageCondition::Captcha(_)
        ));
        let login = page("http://x/l", "请登录后继续", Markers { password: true, ..Default::default() });
        assert!(matches!(classify_page(&login, Some(200)), PageCondition::LoginRequired(_)));
        assert!(matches!(classify_page(&page("http://x/", "x", m.clone()), Some(401)), PageCondition::LoginRequired(_)));
        assert!(matches!(classify_page(&page("http://x/", "Access Denied", m.clone()), Some(200)), PageCondition::Blocked(_)));
        assert!(matches!(classify_page(&page("http://x/", "x", m), Some(429)), PageCondition::Blocked(_)));
    }

    #[test]
    fn clicks_are_classified_conservatively() {
        let p = page("http://x/a", "", Markers::default());
        let mut link = el("a", "", "下一篇");
        link.href = "/next".into();
        assert_eq!(classify_click(&link, &p), ClickKind::Navigate);
        let mut del = el("a", "", "删除账户");
        del.href = "/delete".into();
        assert_eq!(classify_click(&del, &p), ClickKind::Submit, "a link that says delete is a submission");
        assert_eq!(classify_click(&el("button", "submit", "提交"), &p), ClickKind::Submit);
        assert_eq!(classify_click(&el("button", "button", "展开更多"), &p), ClickKind::Submit, "unknown → submit");
        assert_eq!(classify_click(&el("input", "checkbox", "记住我"), &p), ClickKind::Local);
        assert_eq!(classify_click(&el("summary", "", "详情"), &p), ClickKind::Local);
        assert_eq!(classify_click(&el("button", "submit", "立即支付"), &p), ClickKind::Payment);
        let mut pay_page = page("http://shop/checkout", "", Markers::default());
        assert!(is_checkout(&pay_page));
        assert_eq!(classify_click(&el("button", "submit", "继续"), &pay_page), ClickKind::Payment);
        pay_page.url = "http://shop/item".into();
        pay_page.markers.card = true;
        assert!(is_checkout(&pay_page));
    }
}
