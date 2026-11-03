//! Executors for approved browser actions: open a page, click (navigate or
//! submit). They run wherever the approval is executed — in the chat process
//! or in `hatch approvals approve` — and reach the live tab through the
//! broker. A submission is re-verified against the approved page, element and
//! form right before the click.

use crate::builtin::browser::check_owner;
use crate::executors::{ActionExecutor, ExecContext};
use hatch_browser::{Broker, ClickExpect, ClickKind};
use hatch_core::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct OpenPayload {
    pub url: String,
    #[serde(default)]
    pub session: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ClickPayload {
    pub session: String,
    #[serde(rename = "ref")]
    pub reference: u32,
    pub kind: ClickKind,
    pub expect: ClickExpect,
    #[serde(default)]
    pub screenshot: Option<String>,
}

pub struct BrowserOpenExecutor {
    pub broker: Arc<Broker>,
}

impl ActionExecutor for BrowserOpenExecutor {
    fn kind(&self) -> &'static str {
        "browser_open"
    }

    fn execute(&self, payload: &Value, ctx: &ExecContext) -> Result<String> {
        let p: OpenPayload = serde_json::from_value(payload.clone())?;
        let view = match &p.session {
            Some(s) => {
                check_owner(s, ctx.db, ctx.origin)?;
                self.broker.navigate(ctx.db, s, &p.url, ctx.cancel)
            }
            None => self.broker.open(ctx.db, ctx.origin, &p.url, ctx.cancel),
        }
        .map_err(Error::from)?;
        Ok(view.render())
    }
}

pub struct BrowserClickExecutor {
    pub broker: Arc<Broker>,
}

impl ActionExecutor for BrowserClickExecutor {
    fn kind(&self) -> &'static str {
        "browser_click"
    }

    fn execute(&self, payload: &Value, ctx: &ExecContext) -> Result<String> {
        let p: ClickPayload = serde_json::from_value(payload.clone())?;
        if p.kind == ClickKind::Payment {
            return Err(Error::Denied("支付只能由你本人完成，hatch 不会点击支付".into()));
        }
        check_owner(&p.session, ctx.db, ctx.origin)?;
        // Submissions are checked against exactly what was approved; links only against the page and element.
        let expect = if p.kind == ClickKind::Submit { p.expect.clone() } else { ClickExpect { form: vec![], ..p.expect.clone() } };
        let view = self.broker.click(ctx.db, &p.session, p.reference, Some(&expect), ctx.cancel).map_err(Error::from)?;
        let head = if p.kind == ClickKind::Submit { "已提交。提交后的页面：\n" } else { "" };
        Ok(format!("{head}{}", view.render()))
    }
}
