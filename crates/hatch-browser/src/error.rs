//! Typed browser failures.
//!
//! The categories exist so that every report says *why* something did not
//! work: "the page could not be fetched" is not "you need to log in", and
//! neither is "the option does not exist". Callers must never collapse them
//! into "there is nothing".

use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BrowserError {
    /// DNS, connection, TLS or HTTP-level failure.
    Network(String),
    /// The site refused the request (HTTP 403/429, bot wall, "access denied").
    Blocked(String),
    /// A login is required; the user has to take over.
    LoginRequired(String),
    /// A captcha / human check is shown; the user has to take over. Never solved automatically.
    Captcha(String),
    /// No element with that reference (the page changed, or the ref is stale).
    ElementNotFound(String),
    /// The select box exists but has no such option. Lists what is available.
    NoSuchOption { wanted: String, available: Vec<String> },
    Timeout(String),
    Cancelled,
    /// hatch's own policy refused the destination or the action.
    PolicyDenied(String),
    /// The session is waiting for the user (login / captcha) or is closed.
    SessionUnavailable(String),
    /// The browser could not be started or reached.
    Unavailable(String),
    /// Unexpected protocol data.
    Protocol(String),
}

impl BrowserError {
    /// Stable category name, used in tool results and receipts.
    pub fn category(&self) -> &'static str {
        match self {
            BrowserError::Network(_) => "network_error",
            BrowserError::Blocked(_) => "blocked",
            BrowserError::LoginRequired(_) => "login_required",
            BrowserError::Captcha(_) => "captcha",
            BrowserError::ElementNotFound(_) => "element_not_found",
            BrowserError::NoSuchOption { .. } => "no_such_option",
            BrowserError::Timeout(_) => "timeout",
            BrowserError::Cancelled => "cancelled",
            BrowserError::PolicyDenied(_) => "policy_denied",
            BrowserError::SessionUnavailable(_) => "session_unavailable",
            BrowserError::Unavailable(_) => "browser_unavailable",
            BrowserError::Protocol(_) => "protocol_error",
        }
    }
}

impl fmt::Display for BrowserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let c = self.category();
        match self {
            BrowserError::Network(m) => write!(f, "[{c}] 网络或加载失败（不代表页面上没有该内容）：{m}"),
            BrowserError::Blocked(m) => write!(f, "[{c}] 网站拒绝了访问（可能拦截自动化浏览器）：{m}"),
            BrowserError::LoginRequired(m) => write!(f, "[{c}] 需要登录，请用户接管完成登录：{m}"),
            BrowserError::Captcha(m) => write!(f, "[{c}] 页面要求人机验证，hatch 不会尝试破解，请用户接管：{m}"),
            BrowserError::ElementNotFound(m) => write!(f, "[{c}] 找不到该元素（页面可能已变化，请重新读取页面）：{m}"),
            BrowserError::NoSuchOption { wanted, available } => {
                write!(f, "[{c}] 下拉框中没有选项「{wanted}」。现有选项：{}", available.join("、"))
            }
            BrowserError::Timeout(m) => write!(f, "[{c}] 超时：{m}"),
            BrowserError::Cancelled => write!(f, "[{c}] 已取消"),
            BrowserError::PolicyDenied(m) => write!(f, "[{c}] 被安全策略拒绝：{m}"),
            BrowserError::SessionUnavailable(m) => write!(f, "[{c}] 会话不可用：{m}"),
            BrowserError::Unavailable(m) => write!(f, "[{c}] 浏览器不可用：{m}"),
            BrowserError::Protocol(m) => write!(f, "[{c}] 浏览器协议错误：{m}"),
        }
    }
}

impl std::error::Error for BrowserError {}

impl From<BrowserError> for hatch_core::Error {
    fn from(e: BrowserError) -> Self {
        match e {
            BrowserError::Cancelled => hatch_core::Error::Cancelled("浏览器操作已取消".into()),
            BrowserError::PolicyDenied(_) => hatch_core::Error::Denied(e.to_string()),
            BrowserError::ElementNotFound(_) | BrowserError::NoSuchOption { .. } | BrowserError::SessionUnavailable(_) => {
                hatch_core::Error::Invalid(e.to_string())
            }
            _ => hatch_core::Error::Connector(e.to_string()),
        }
    }
}

pub type BResult<T> = std::result::Result<T, BrowserError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories_are_distinct_and_honest() {
        let all = [
            BrowserError::Network("x".into()),
            BrowserError::Blocked("x".into()),
            BrowserError::LoginRequired("x".into()),
            BrowserError::Captcha("x".into()),
            BrowserError::ElementNotFound("x".into()),
            BrowserError::NoSuchOption { wanted: "拉萨".into(), available: vec!["北京".into(), "上海".into()] },
            BrowserError::Timeout("x".into()),
            BrowserError::Cancelled,
            BrowserError::PolicyDenied("x".into()),
            BrowserError::SessionUnavailable("x".into()),
            BrowserError::Unavailable("x".into()),
            BrowserError::Protocol("x".into()),
        ];
        let mut cats: Vec<&str> = all.iter().map(BrowserError::category).collect();
        cats.sort();
        cats.dedup();
        assert_eq!(cats.len(), all.len());
        assert!(all[0].to_string().contains("不代表页面上没有"));
        assert!(all[5].to_string().contains("北京、上海"));
        assert!(matches!(hatch_core::Error::from(BrowserError::Cancelled), hatch_core::Error::Cancelled(_)));
    }
}
