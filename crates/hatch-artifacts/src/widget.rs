//! Widget protocol: cards the assistant can offer, inert until activated.
//!
//! A widget is a small JSON card — `options` (a list of choices),
//! `list` (rows with title/subtitle/link) or `html` (a sanitised HTML
//! card). Every widget carries an `embed_token`.
//!
//! # The embed-token rule
//!
//! A widget is **inert** until its token appears in a user-visible
//! message. [`render`] encodes this: with `token_present == false` it
//! returns only the plain-text fallback (no card); with the token present
//! it returns the card JSON as well. The CLI has no UI, so it always
//! renders with `token_present == false` — the text fallback is the whole
//! rendering there, and it loses no information: every option label,
//! reply, row and link is present as text.
//!
//! The token rule is enforced at the rendering site, not on the wire:
//! the payload always travels with its token (in MCP
//! `structuredContent.widget`, next to the existing `trust` label), and a
//! conforming client renders the card only when the token is in the
//! message text. A client that renders the card without the token is
//! violating the protocol, not a bug in the payload. This is the one
//! place the rule cannot be enforced server-side — see SECURITY.md §9f.
//!
//! # HTML sanitiser
//!
//! [`sanitize_html`] is an allowlist sanitiser (no `<script>`, no inline
//! event handlers, no external fetches). Its exact rules are documented
//! on the function; what it does *not* guarantee is documented in
//! SECURITY.md §9f.

use crate::sanitize::sanitize_html;
use hatch_core::{ids, Error, Result};
use serde_json::{json, Value};

/// One choice in an `options` widget.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OptionItem {
    pub id: String,
    pub label: String,
    pub reply: String,
}

/// One row in a `list` widget.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListRow {
    pub title: String,
    pub subtitle: String,
    /// Optional link (relative path or `#anchor` only in rendered HTML;
    /// the text fallback prints it verbatim).
    pub link: Option<String>,
}

/// The three widget kinds. `html` is stored already sanitised.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WidgetKind {
    Options { title: String, options: Vec<OptionItem> },
    List { title: String, rows: Vec<ListRow> },
    Html { title: String, html: String },
}

impl WidgetKind {
    pub fn type_name(&self) -> &'static str {
        match self {
            WidgetKind::Options { .. } => "options",
            WidgetKind::List { .. } => "list",
            WidgetKind::Html { .. } => "html",
        }
    }
}

/// A widget: a kind plus its activation token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Widget {
    pub kind: WidgetKind,
    pub embed_token: String,
}

impl Widget {
    /// Build a widget, minting a fresh token.
    pub fn new(kind: WidgetKind) -> Self {
        Widget { kind, embed_token: ids::random_id("wgt") }
    }

    /// Serialise for the wire (MCP `structuredContent.widget`, tool
    /// results). The token travels with the payload; see module docs.
    pub fn to_json(&self) -> Value {
        let mut v = match &self.kind {
            WidgetKind::Options { title, options } => json!({
                "type": "options", "title": title,
                "options": options.iter().map(|o| json!({"id": o.id, "label": o.label, "reply": o.reply})).collect::<Vec<_>>(),
            }),
            WidgetKind::List { title, rows } => json!({
                "type": "list", "title": title,
                "rows": rows.iter().map(|r| json!({"title": r.title, "subtitle": r.subtitle, "link": r.link})).collect::<Vec<_>>(),
            }),
            WidgetKind::Html { title, html } => json!({
                "type": "html", "title": title, "html": html,
            }),
        };
        v["embed_token"] = json!(self.embed_token);
        v
    }

    /// Parse and validate a widget JSON value. Bad `type`, missing fields
    /// and empty option/row lists are errors. HTML is sanitised on the
    /// way in; a supplied `embed_token` is kept, otherwise one is minted.
    pub fn from_json(value: &Value) -> Result<Widget> {
        let obj = value.as_object().ok_or_else(|| Error::Invalid("widget must be a JSON object".into()))?;
        let ty = obj
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Invalid("widget is missing string field `type`".into()))?;
        let title = obj
            .get("title")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| Error::Invalid("widget is missing non-empty string field `title`".into()))?
            .to_string();
        let kind = match ty {
            "options" => {
                let arr = obj
                    .get("options")
                    .and_then(Value::as_array)
                    .ok_or_else(|| Error::Invalid("options widget needs an `options` array".into()))?;
                if arr.is_empty() {
                    return Err(Error::Invalid("options widget needs at least one option".into()));
                }
                let mut options = Vec::new();
                for (i, o) in arr.iter().enumerate() {
                    let get = |k: &str| {
                        o.get(k)
                            .and_then(Value::as_str)
                            .filter(|s| !s.trim().is_empty())
                            .map(str::to_string)
                            .ok_or_else(|| Error::Invalid(format!("options[{i}] is missing non-empty string field `{k}`")))
                    };
                    options.push(OptionItem { id: get("id")?, label: get("label")?, reply: get("reply")? });
                }
                WidgetKind::Options { title, options }
            }
            "list" => {
                let arr = obj
                    .get("rows")
                    .and_then(Value::as_array)
                    .ok_or_else(|| Error::Invalid("list widget needs a `rows` array".into()))?;
                if arr.is_empty() {
                    return Err(Error::Invalid("list widget needs at least one row".into()));
                }
                let mut rows = Vec::new();
                for (i, r) in arr.iter().enumerate() {
                    let title = r
                        .get("title")
                        .and_then(Value::as_str)
                        .filter(|s| !s.trim().is_empty())
                        .map(str::to_string)
                        .ok_or_else(|| Error::Invalid(format!("rows[{i}] is missing non-empty string field `title`")))?;
                    let subtitle = r.get("subtitle").and_then(Value::as_str).unwrap_or("").to_string();
                    let link = r.get("link").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
                    rows.push(ListRow { title, subtitle, link });
                }
                WidgetKind::List { title, rows }
            }
            "html" => {
                let html = obj
                    .get("html")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::Invalid("html widget needs a string field `html`".into()))?;
                WidgetKind::Html { title, html: sanitize_html(html) }
            }
            other => return Err(Error::Invalid(format!("unknown widget type `{other}` (options | list | html)"))),
        };
        let token = obj
            .get("embed_token")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| ids::random_id("wgt"));
        Ok(Widget { kind, embed_token: token })
    }
}

/// What [`render`] returns: the text fallback is always present; the card
/// JSON is present only when the embed token was present.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rendered {
    pub text: String,
    pub card: Option<Value>,
}

/// Render a widget. Without the token the widget is inert: plain text
/// only, no card. With the token the card JSON is included alongside the
/// same text. The text fallback never loses information.
pub fn render(widget: &Widget, token_present: bool) -> Rendered {
    Rendered { text: text_fallback(widget), card: token_present.then(|| widget.to_json()) }
}

/// Plain-text rendering of a widget. Every option label and reply, every
/// row title/subtitle/link, and (for HTML) the visible text are present.
pub fn text_fallback(widget: &Widget) -> String {
    match &widget.kind {
        WidgetKind::Options { title, options } => {
            let mut s = format!("{title}\n");
            for (i, o) in options.iter().enumerate() {
                s.push_str(&format!("{}. {}（{}）— 回复：{}\n", i + 1, o.label, o.id, o.reply));
            }
            s.trim_end().to_string()
        }
        WidgetKind::List { title, rows } => {
            let mut s = format!("{title}\n");
            for r in rows {
                s.push_str(&format!("· {}", r.title));
                if !r.subtitle.is_empty() {
                    s.push_str(&format!(" — {}", r.subtitle));
                }
                if let Some(l) = &r.link {
                    s.push_str(&format!(" [{l}]"));
                }
                s.push('\n');
            }
            s.trim_end().to_string()
        }
        WidgetKind::Html { title, html } => {
            let text = strip_tags_to_text(html);
            if text.is_empty() { title.clone() } else { format!("{title}\n{text}") }
        }
    }
}

/// Strip tags from (already sanitised) HTML to plain text.
pub fn strip_tags_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut prev_space = true;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                if !prev_space {
                    out.push(' ');
                    prev_space = true;
                }
            }
            _ if !in_tag => {
                let is_space = c.is_whitespace();
                if is_space && prev_space {
                    continue;
                }
                out.push(if is_space { ' ' } else { c });
                prev_space = is_space;
            }
            _ => {}
        }
    }
    out.trim().to_string()
}


#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn options_widget() -> Widget {
        Widget::from_json(&json!({
            "type": "options", "title": "选一个",
            "options": [
                {"id": "a", "label": "方案 A", "reply": "选 A"},
                {"id": "b", "label": "方案 B", "reply": "选 B"}
            ]
        }))
        .unwrap()
    }

    #[test]
    fn schema_rejects_bad_type_and_missing_fields() {
        assert!(Widget::from_json(&json!({"type": "nope", "title": "t"})).is_err());
        assert!(Widget::from_json(&json!({"type": "options", "title": "t"})).is_err());
        assert!(Widget::from_json(&json!({"type": "options", "title": "t", "options": []})).is_err());
        assert!(Widget::from_json(&json!({"type": "options", "title": "t", "options": [{"id": "a", "label": "A"}]})).is_err());
        assert!(Widget::from_json(&json!({"type": "list", "title": "t", "rows": []})).is_err());
        assert!(Widget::from_json(&json!({"type": "html", "title": "t"})).is_err());
        assert!(Widget::from_json(&json!({"title": "t"})).is_err());
        assert!(Widget::from_json(&json!({"type": "options"})).is_err());
        assert!(Widget::from_json(&json!("not an object")).is_err());
    }

    #[test]
    fn embed_token_inert_without_token_active_with_token() {
        let w = options_widget();
        let inert = render(&w, false);
        assert!(inert.card.is_none(), "no card without the token");
        assert!(inert.text.contains("方案 A") && inert.text.contains("方案 B"));
        let active = render(&w, true);
        let card = active.card.expect("card with the token");
        assert_eq!(card["type"], "options");
        assert_eq!(card["embed_token"], w.embed_token);
        assert_eq!(active.text, inert.text, "text fallback is identical either way");
    }

    #[test]
    fn text_fallback_contains_every_option_and_link() {
        let w = Widget::from_json(&json!({
            "type": "list", "title": "文档",
            "rows": [
                {"title": "周报", "subtitle": "第 3 周", "link": "artifacts/art1/v1/content.md"},
                {"title": "笔记", "subtitle": "", "link": "#top"}
            ]
        }))
        .unwrap();
        let text = text_fallback(&w);
        assert!(text.contains("周报") && text.contains("第 3 周"));
        assert!(text.contains("artifacts/art1/v1/content.md"));
        assert!(text.contains("笔记") && text.contains("#top"));
        let o = options_widget();
        let t = text_fallback(&o);
        assert!(t.contains("方案 A") && t.contains("选 A") && t.contains("方案 B") && t.contains("选 B"));
    }


    #[test]
    fn html_widget_is_sanitised_on_parse() {
        let w = Widget::from_json(&json!({"type": "html", "title": "卡片", "html": "<p>好</p><script>坏</script>"})).unwrap();
        let html = w.to_json()["html"].as_str().expect("html field").to_string();
        assert!(html.contains("好") && !html.contains("script"));
        assert!(strip_tags_to_text(&html).contains("好"));
    }
}
