//! What an action returns to the caller (and, rendered, to the model).

use crate::detect::PageCondition;
use crate::snapshot::Snapshot;

/// Text budget for page content shown to the model.
pub const VIEW_TEXT_CHARS: usize = 6000;
pub const VIEW_ELEMENTS: usize = 80;

#[derive(Clone, Debug)]
pub struct PageView {
    pub session: String,
    pub profile: String,
    pub status: Option<u16>,
    pub condition: PageCondition,
    pub snapshot: Snapshot,
    /// Requests refused by the destination policy during this action.
    pub blocked: Vec<String>,
    /// JavaScript dialogs that were dismissed.
    pub dialogs: Vec<String>,
    /// The action loaded a new document.
    pub navigated: bool,
}

/// Instructions shown when the user has to take over a session.
pub fn takeover_hint(session: &str) -> String {
    format!(
        "会话 {session} 已暂停，等待你接管：在你的电脑上运行 `hatch browser takeover {session}` 用同一个浏览器 profile 打开该页面，\
         完成后运行 `hatch browser resume {session}`，再让助手继续。"
    )
}

impl PageView {
    /// Banner + snapshot for the model. Page content is untrusted; callers
    /// wrap it accordingly.
    pub fn render(&self) -> String {
        let mut s = format!("浏览器会话 {}（profile {}）", self.session, self.profile);
        if let Some(code) = self.status {
            s.push_str(&format!("，HTTP {code}"));
        }
        s.push('\n');
        match &self.condition {
            PageCondition::Ok => {}
            PageCondition::LoginRequired(d) => {
                s.push_str(&format!("⚠ [login_required] 需要登录（{d}）。hatch 不会代填密码。{}\n", takeover_hint(&self.session)));
            }
            PageCondition::Captcha(d) => {
                s.push_str(&format!("⚠ [captcha] 页面要求人机验证（{d}）。hatch 不会尝试破解验证码。{}\n", takeover_hint(&self.session)));
            }
            PageCondition::Blocked(d) => {
                s.push_str(&format!("⚠ [blocked] 网站拒绝了访问（{d}）。这不代表页面上没有相关内容；可以换一个来源。\n"));
            }
        }
        for b in &self.blocked {
            s.push_str(&format!("⚠ [policy_denied] 已拦截请求：{b}\n"));
        }
        for d in &self.dialogs {
            s.push_str(&format!("（已关闭页面弹出的对话框：{d}）\n"));
        }
        s.push('\n');
        s.push_str(&self.snapshot.render(VIEW_TEXT_CHARS, VIEW_ELEMENTS));
        s
    }

    pub fn needs_user(&self) -> bool {
        matches!(self.condition, PageCondition::LoginRequired(_) | PageCondition::Captcha(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banners() {
        let mut v = PageView {
            session: "bs_1".into(),
            profile: "main".into(),
            status: Some(200),
            condition: PageCondition::Captcha("x".into()),
            snapshot: Snapshot { url: "http://x/".into(), title: "t".into(), ..Default::default() },
            blocked: vec!["http://10.0.0.1/ — 不允许".into()],
            dialogs: vec![],
            navigated: true,
        };
        let r = v.render();
        assert!(r.contains("[captcha]") && r.contains("hatch browser takeover bs_1") && r.contains("[policy_denied]"));
        assert!(v.needs_user());
        v.condition = PageCondition::Blocked("HTTP 403".into());
        assert!(v.render().contains("不代表页面上没有") && !v.needs_user());
    }
}
