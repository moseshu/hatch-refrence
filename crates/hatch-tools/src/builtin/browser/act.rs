//! Acting on pages: click, type, select.
//!
//! What a click *is* (navigation, page widget, submission, payment) is
//! decided here by code from the element and the page — never by the model.

use super::{err, notify_if_needed, own_session, ref_arg};
use crate::browser_exec::ClickPayload;
use crate::tool::{arg_str, PlannedAction, Tool, ToolContext, ToolOutput};
use chrono::Duration;
use hatch_browser::{classify_click, Broker, ClickExpect, ClickKind, Element, FormField, Snapshot};
use hatch_core::{ids, Result};
use hatch_llm::ToolSpec;
use hatch_policy::{ActionClass, DataLabel, NewApproval};
use serde_json::{json, Value};
use std::sync::Arc;

pub struct BrowserClick {
    pub broker: Arc<Broker>,
}

fn class_of(kind: ClickKind) -> ActionClass {
    match kind {
        ClickKind::Navigate => ActionClass::FetchWeb,
        ClickKind::Local => ActionClass::BrowseLocal,
        ClickKind::Submit => ActionClass::BrowserSubmit,
        ClickKind::Payment => ActionClass::Payment,
    }
}

fn kind_label(kind: ClickKind) -> &'static str {
    match kind {
        ClickKind::Navigate => "打开链接",
        ClickKind::Local => "页面内操作",
        ClickKind::Submit => "提交",
        ClickKind::Payment => "支付",
    }
}

impl BrowserClick {
    fn inspect(&self, args: &Value, ctx: &ToolContext) -> Result<(String, u32, Element, Snapshot, ClickKind)> {
        let id = own_session(args, ctx)?.to_string();
        let r = ref_arg(args)?;
        let (el, page) = self.broker.describe(ctx.db, &id, r, Some(&ctx.cancel)).map_err(err)?;
        let kind = classify_click(&el, &page);
        Ok((id, r, el, page, kind))
    }
}

fn render_form(form: &[FormField]) -> String {
    if form.is_empty() {
        return "（没有表单字段）".into();
    }
    form.iter().map(|f| format!("  {}：{}", if f.label.is_empty() { &f.name } else { &f.label }, f.value)).collect::<Vec<_>>().join("\n")
}

impl Tool for BrowserClick {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "browser_click".into(),
            description: "点击浏览器页面上编号为 ref 的元素（编号来自 browser_read）。普通链接直接打开；任何可能向网站提交内容的按钮\
                          （提交、发送、确认、删除等）都会先进入审批，用户批准后才执行；支付只能建议，不会执行。"
                .into(),
            parameters: json!({"type": "object", "properties": {
                "session": {"type": "string"},
                "ref": {"type": "integer"}
            }, "required": ["session", "ref"]}),
        }
    }

    fn class(&self) -> ActionClass {
        ActionClass::BrowserSubmit
    }

    fn plan(&self, args: &Value, ctx: &ToolContext) -> Result<PlannedAction> {
        let (id, _, el, page, kind) = self.inspect(args, ctx)?;
        let mut labels = Vec::new();
        if kind == ClickKind::Submit {
            let form = self.broker.form_fields(ctx.db, &id, ref_arg(args)?, Some(&ctx.cancel)).map_err(err)?;
            if form.iter().any(|f| hatch_core::secrets::contains_secret(&f.value)) {
                labels.push(DataLabel::Secret);
            }
        }
        let host = hatch_core::netguard::parse_http_url(&page.url).map(|p| p.host).unwrap_or_default();
        Ok(PlannedAction {
            class: Some(class_of(kind)),
            summary: format!("{}：「{}」（{}）", kind_label(kind), el.display_name(), page.url),
            recipients: if kind == ClickKind::Local { vec![] } else { vec![host] },
            labels,
        })
    }

    /// Page widgets run directly.
    fn call(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let (id, r, _, _, kind) = self.inspect(args, ctx)?;
        if kind != ClickKind::Local {
            return Err(hatch_core::Error::Invalid("this click runs through the action queue".into()));
        }
        let v = self.broker.click(ctx.db, &id, r, None, Some(&ctx.cancel)).map_err(err)?;
        notify_if_needed(ctx.db, ctx.tz, &v, &ctx.now);
        Ok(ToolOutput::untrusted(v.render()))
    }

    /// Links, submissions and payments go through gate → approval → executor.
    fn to_approval(&self, args: &Value, ctx: &ToolContext) -> Result<Option<NewApproval>> {
        let (id, r, el, page, kind) = self.inspect(args, ctx)?;
        if kind == ClickKind::Local {
            return Ok(None);
        }
        let form = if matches!(kind, ClickKind::Submit | ClickKind::Payment) {
            self.broker.form_fields(ctx.db, &id, r, Some(&ctx.cancel)).map_err(err)?
        } else {
            Vec::new()
        };
        let screenshot = if kind == ClickKind::Submit {
            self.broker.screenshot(ctx.db, &id, Some(&ctx.cancel)).ok().map(|p| p.display().to_string())
        } else {
            None
        };
        let class = class_of(kind);
        let mut preview = format!("页面：{}\n动作：{}「{}」\n元素：{}\n", page.url, kind_label(kind), el.display_name(), el.render());
        if matches!(kind, ClickKind::Submit | ClickKind::Payment) {
            preview.push_str(&format!("将提交的表单内容：\n{}\n", render_form(&form)));
        }
        if let Some(s) = &screenshot {
            preview.push_str(&format!("截图：{s}\n"));
        }
        if kind == ClickKind::Submit {
            preview.push_str("批准后执行前会再次核对页面、按钮和表单内容；有任何变化都不会点击。");
        }
        let expect = ClickExpect { page_url: page.url.clone(), element: el.clone(), form: form.clone() };
        let form_sig: Vec<String> = form.iter().map(|f| format!("{}={}", f.name, f.value)).collect();
        let payload = serde_json::to_value(ClickPayload { session: id.clone(), reference: r, kind, expect, screenshot })?;
        Ok(Some(NewApproval {
            idempotency_key: ids::stable_id("bclick", &[&ctx.origin, &id, &page.url, &r.to_string(), &el.display_name(), &form_sig.join("&")]),
            kind: "browser_click".into(),
            class,
            title: format!("浏览器{}：「{}」", kind_label(kind), el.display_name().chars().take(40).collect::<String>()),
            preview,
            payload,
            reasons: vec![],
            origin: ctx.origin.clone(),
            ttl: Duration::hours(2),
        }))
    }
}

pub struct BrowserType {
    pub broker: Arc<Broker>,
}

impl Tool for BrowserType {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "browser_type".into(),
            description: "在浏览器页面编号为 ref 的输入框中填写文字（会先清空原内容）。只是填写，不会提交；提交要点击提交按钮并经用户批准。\
                          不能填写密码框：遇到登录请让用户接管。"
                .into(),
            parameters: json!({"type": "object", "properties": {
                "session": {"type": "string"},
                "ref": {"type": "integer"},
                "text": {"type": "string"}
            }, "required": ["session", "ref", "text"]}),
        }
    }

    fn class(&self) -> ActionClass {
        ActionClass::BrowseLocal
    }

    fn plan(&self, args: &Value, _ctx: &ToolContext) -> Result<PlannedAction> {
        let text = args.get("text").and_then(Value::as_str).unwrap_or("");
        let mut labels = Vec::new();
        if hatch_core::secrets::contains_secret(text) {
            labels.push(DataLabel::Secret);
        }
        Ok(PlannedAction { summary: "在页面输入框中填写".into(), labels, ..Default::default() })
    }

    fn call(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let id = own_session(args, ctx)?;
        let text = args.get("text").and_then(Value::as_str).unwrap_or("");
        let el = self.broker.type_text(ctx.db, id, ref_arg(args)?, text, Some(&ctx.cancel)).map_err(err)?;
        Ok(ToolOutput::system(format!("已在「{}」中填写，当前值：{}（尚未提交）", el.display_name(), el.value)))
    }
}

pub struct BrowserSelect {
    pub broker: Arc<Broker>,
}

impl Tool for BrowserSelect {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "browser_select".into(),
            description: "在浏览器页面编号为 ref 的下拉框中按显示文字选择一个选项（只是选择，不会提交）。选项不存在时会列出现有选项。".into(),
            parameters: json!({"type": "object", "properties": {
                "session": {"type": "string"},
                "ref": {"type": "integer"},
                "option": {"type": "string"}
            }, "required": ["session", "ref", "option"]}),
        }
    }

    fn class(&self) -> ActionClass {
        ActionClass::BrowseLocal
    }

    fn call(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let id = own_session(args, ctx)?;
        let chosen = self.broker.select(ctx.db, id, ref_arg(args)?, arg_str(args, "option")?, Some(&ctx.cancel)).map_err(err)?;
        Ok(ToolOutput::system(format!("已选择「{chosen}」（尚未提交）")))
    }
}
