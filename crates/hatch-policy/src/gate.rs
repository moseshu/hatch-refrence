//! The gatekeeper: decides Allow / RequireApproval / Deny for every action.
//!
//! The decision is a pure function of the configured autonomy, the request and
//! the active grants. Rules, in order:
//!
//! 1. Carrying a `Secret` label → **Deny**.
//! 2. Autonomy `Suggest` (payments by default and by ceiling) → **Deny** (suggest only).
//! 3. Collect approval reasons:
//!    * autonomy below `ActNotify` (irreversible classes by default);
//!    * **taint rule** — external effect while the context holds untrusted content;
//!    * **flow rule** — new recipients, or sensitive labels leaving the boundary.
//! 4. No reasons → **Allow**. Otherwise a matching grant may waive the default,
//!    the new-recipient reason (its patterns name them) and — only with
//!    `allow_when_tainted` — the taint reason. Sensitive-data reasons are never
//!    waived by a grant.
//!
//! Moses <moseshu25@gmail.com>

use crate::action::{ActionClass, Autonomy, DataLabel};
use crate::grants::Grant;
use hatch_core::time::Timestamp;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize)]
pub struct PolicyConfig {
    autonomy: BTreeMap<ActionClass, Autonomy>,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        PolicyConfig { autonomy: ActionClass::ALL.into_iter().map(|c| (c, c.default_autonomy())).collect() }
    }
}

impl PolicyConfig {
    pub fn level(&self, c: ActionClass) -> Autonomy {
        self.autonomy.get(&c).copied().unwrap_or(c.default_autonomy())
    }

    /// Set a level, clamped to the class ceiling. Returns the effective level.
    pub fn set(&mut self, c: ActionClass, level: Autonomy) -> Autonomy {
        let eff = level.min(c.autonomy_ceiling());
        self.autonomy.insert(c, eff);
        eff
    }
}

/// Everything the gate needs to know about a proposed action.
#[derive(Clone, Debug, Default)]
pub struct ActionRequest {
    pub class: Option<ActionClass>,
    pub summary: String,
    pub recipients: Vec<String>,
    /// Recipients that have no people page (computed by the caller from memory).
    pub unknown_recipients: Vec<String>,
    pub labels: Vec<DataLabel>,
    /// The context that produced this action contains untrusted content.
    pub tainted: bool,
}

impl ActionRequest {
    pub fn new(class: ActionClass, summary: impl Into<String>) -> Self {
        ActionRequest { class: Some(class), summary: summary.into(), ..Default::default() }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Decision {
    Allow { notify: bool, grant: Option<String> },
    RequireApproval { reasons: Vec<String> },
    Deny { reason: String },
}

#[derive(Clone, Debug, PartialEq)]
enum Reason {
    Default(ActionClass),
    Tainted,
    NewRecipients(Vec<String>),
    Sensitive(Vec<DataLabel>),
}

impl Reason {
    fn text(&self) -> String {
        match self {
            Reason::Default(c) => format!("{} 默认需要用户确认（{:?}）", c.as_str(), c.reversibility()),
            Reason::Tainted => "该动作源自外部不可信内容（如来信），外发前必须由你确认".into(),
            Reason::NewRecipients(r) => format!("收件人不在联系人中：{}", r.join(", ")),
            Reason::Sensitive(l) => format!("将向外发送敏感数据：{l:?}"),
        }
    }
}

pub struct Gatekeeper {
    pub config: PolicyConfig,
}

impl Gatekeeper {
    pub fn new(config: PolicyConfig) -> Self {
        Gatekeeper { config }
    }

    pub fn evaluate(&self, req: &ActionRequest, grants: &[Grant], now: &Timestamp) -> Decision {
        let Some(class) = req.class else {
            return Decision::Deny { reason: "action has no class".into() };
        };
        if req.labels.contains(&DataLabel::Secret) {
            return Decision::Deny { reason: "包含密码/验证码等秘密信息，禁止外发或存储".into() };
        }
        let level = self.config.level(class);
        if level == Autonomy::Suggest {
            return Decision::Deny { reason: format!("{} 只能由助手提出建议，必须由你本人执行", class.as_str()) };
        }
        let mut reasons = Vec::new();
        if level < Autonomy::ActNotify {
            reasons.push(Reason::Default(class));
        }
        if req.tainted && class.is_external() {
            reasons.push(Reason::Tainted);
        }
        if class == ActionClass::SendMessage {
            if !req.unknown_recipients.is_empty() {
                reasons.push(Reason::NewRecipients(req.unknown_recipients.clone()));
            }
            let sensitive: Vec<DataLabel> = req
                .labels
                .iter()
                .copied()
                .filter(|l| matches!(l, DataLabel::CalendarDetails | DataLabel::OtherPeoplesMessages | DataLabel::MemoryPrivate))
                .collect();
            if !sensitive.is_empty() {
                reasons.push(Reason::Sensitive(sensitive));
            }
        }
        if reasons.is_empty() {
            return Decision::Allow { notify: level == Autonomy::ActNotify, grant: None };
        }
        for g in grants.iter().filter(|g| g.class == class && g.is_active(now) && g.covers(&req.recipients)) {
            let waived = reasons.iter().all(|r| match r {
                Reason::Default(_) | Reason::NewRecipients(_) => true,
                Reason::Tainted => g.allow_when_tainted,
                Reason::Sensitive(_) => false,
            });
            if waived {
                return Decision::Allow { notify: true, grant: Some(g.id.clone()) };
            }
        }
        Decision::RequireApproval { reasons: reasons.iter().map(Reason::text).collect() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(tainted_ok: bool) -> Grant {
        Grant {
            id: "g1".into(),
            class: ActionClass::SendMessage,
            recipients: vec!["*@corp.com".into()],
            max_uses: None,
            used: 0,
            allow_when_tainted: tainted_ok,
            expires_at: None,
            created_at: hatch_core::time::now(),
            revoked: false,
            note: String::new(),
        }
    }

    fn send(to: &str) -> ActionRequest {
        ActionRequest { recipients: vec![to.into()], ..ActionRequest::new(ActionClass::SendMessage, "reply") }
    }

    #[test]
    fn reads_allowed_sends_need_approval_payments_denied() {
        let g = Gatekeeper::new(PolicyConfig::default());
        let now = hatch_core::time::now();
        assert_eq!(
            g.evaluate(&ActionRequest::new(ActionClass::ReadPersonal, "r"), &[], &now),
            Decision::Allow { notify: false, grant: None }
        );
        assert!(matches!(g.evaluate(&send("a@corp.com"), &[], &now), Decision::RequireApproval { .. }));
        assert!(matches!(g.evaluate(&ActionRequest::new(ActionClass::Payment, "pay"), &[], &now), Decision::Deny { .. }));
    }

    #[test]
    fn secret_label_is_denied_even_with_grant() {
        let g = Gatekeeper::new(PolicyConfig::default());
        let mut r = send("a@corp.com");
        r.labels.push(DataLabel::Secret);
        assert!(matches!(g.evaluate(&r, &[grant(true)], &hatch_core::time::now()), Decision::Deny { .. }));
    }

    #[test]
    fn grants_waive_default_but_not_taint_unless_opted_in() {
        let g = Gatekeeper::new(PolicyConfig::default());
        let now = hatch_core::time::now();
        assert_eq!(
            g.evaluate(&send("a@corp.com"), &[grant(false)], &now),
            Decision::Allow { notify: true, grant: Some("g1".into()) }
        );
        let mut tainted = send("a@corp.com");
        tainted.tainted = true;
        assert!(matches!(g.evaluate(&tainted, &[grant(false)], &now), Decision::RequireApproval { .. }));
        assert!(matches!(g.evaluate(&tainted, &[grant(true)], &now), Decision::Allow { .. }));
        // Grant does not cover an outside domain.
        assert!(matches!(g.evaluate(&send("x@evil.example"), &[grant(true)], &now), Decision::RequireApproval { .. }));
    }

    #[test]
    fn flow_rules_survive_raised_autonomy() {
        let mut cfg = PolicyConfig::default();
        // Ceiling clamps irreversible sends to Draft.
        assert_eq!(cfg.set(ActionClass::SendMessage, Autonomy::Silent), Autonomy::Draft);
        assert_eq!(cfg.set(ActionClass::WriteCalendar, Autonomy::ActNotify), Autonomy::ActNotify);
        let g = Gatekeeper::new(cfg);
        let now = hatch_core::time::now();
        let mut r = ActionRequest::new(ActionClass::WriteCalendar, "add event");
        assert_eq!(g.evaluate(&r, &[], &now), Decision::Allow { notify: true, grant: None });
        r.tainted = true;
        let Decision::RequireApproval { reasons } = g.evaluate(&r, &[], &now) else { panic!() };
        assert!(reasons[0].contains("不可信"));
        let mut s = send("new@corp.com");
        s.unknown_recipients = vec!["new@corp.com".into()];
        s.labels = vec![DataLabel::CalendarDetails];
        let Decision::RequireApproval { reasons } = g.evaluate(&s, &[grant(true)], &now) else { panic!() };
        assert_eq!(reasons.len(), 3);
    }
}
