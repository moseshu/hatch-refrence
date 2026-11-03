//! Page snapshots: readable text plus numbered interactive elements.
//!
//! The snapshot script is fixed and internal. It tags each visible
//! interactive element with `data-hatch-ref="N"`; actions name elements by
//! that number. The model never gets to run JavaScript.

use serde::{Deserialize, Serialize};

/// Shared helpers: visibility, text cleanup, labels and the per-element description.
const HELPERS: &str = r#"
  const vis = el => { const r = el.getBoundingClientRect(); const s = getComputedStyle(el);
    return s.visibility !== 'hidden' && s.display !== 'none' && (r.width > 0 || r.height > 0); };
  const clean = s => (s || '').replace(/\s+/g, ' ').trim();
  const label = el => {
    if (el.labels && el.labels.length) {
      const lb = el.labels[0].cloneNode(true);
      lb.querySelectorAll('select,input,textarea,button').forEach(x => x.remove());
      return clean(lb.textContent);
    }
    return clean(el.getAttribute('aria-label') || el.getAttribute('placeholder') || el.getAttribute('title') || '');
  };
  const describe = (el, n) => {
    const tag = el.tagName.toLowerCase(); const type = (el.getAttribute('type') || '').toLowerCase();
    const form = el.form || el.closest('form');
    const o = { ref: n, tag, type, role: el.getAttribute('role') || '', name: el.getAttribute('name') || '', id: el.id || '',
      label: label(el).slice(0, 120), text: (tag === 'select' || tag === 'textarea') ? '' : clean(el.innerText || (tag === 'input' && ['submit', 'button'].includes(type) ? el.value : '')).slice(0, 120),
      href: el.getAttribute('href') || '', value: '', disabled: !!el.disabled, in_form: !!form,
      form_action: form ? (form.getAttribute('action') || '') : '', form_method: form ? (form.getAttribute('method') || 'get').toLowerCase() : '',
      autocomplete: (el.getAttribute('autocomplete') || '').toLowerCase(), expanded: el.hasAttribute('aria-expanded'), checked: !!el.checked, options: [] };
    if (tag === 'select') { o.options = Array.from(el.options).map(x => clean(x.text)).slice(0, 200); o.value = el.selectedIndex >= 0 ? clean(el.options[el.selectedIndex].text) : ''; }
    else if (type !== 'password' && (tag === 'input' || tag === 'textarea')) o.value = String(el.value || '').slice(0, 200);
    return o;
  };
  const byRef = n => document.querySelector('[data-hatch-ref="' + n + '"]');
"#;

const SNAPSHOT_BODY: &str = r#"
  document.querySelectorAll('[data-hatch-ref]').forEach(e => e.removeAttribute('data-hatch-ref'));
  const sel = 'a[href],button,input,select,textarea,[role=button],[role=link],[role=tab],[role=checkbox],[role=menuitem],[onclick],summary,[contenteditable=true]';
  const els = []; let n = 0;
  for (const el of document.querySelectorAll(sel)) {
    if (el.tagName === 'INPUT' && (el.getAttribute('type') || '').toLowerCase() === 'hidden') continue;
    if (!vis(el) || els.length >= 300) continue;
    n += 1; el.setAttribute('data-hatch-ref', String(n));
    els.push(describe(el, n));
  }
  const text = clean(document.body ? document.body.innerText.replace(/\n+/g, ' ⏎ ') : '').slice(0, 30000);
  const markers = {
    captcha: !!document.querySelector('.g-recaptcha,iframe[src*="recaptcha"],.h-captcha,iframe[src*="hcaptcha"],.cf-turnstile,iframe[src*="challenges.cloudflare"],[data-sitekey]'),
    password: !!document.querySelector('input[type=password]'),
    card: !!document.querySelector('input[autocomplete^="cc-"],input[name*="card" i],input[id*="card" i],input[name*="cvv" i]')
  };
  return JSON.stringify({ url: location.href, title: document.title, text, elements: els, markers });
"#;

fn wrap(body: &str) -> String {
    let mut s = String::from("(() => {");
    s.push_str(HELPERS);
    s.push_str(body);
    s.push_str("})()");
    s
}

/// Tag the page's interactive elements and return the snapshot JSON.
pub fn snapshot_js() -> String {
    wrap(SNAPSHOT_BODY)
}

/// Describe the element already tagged `ref` (does not re-tag the page).
pub fn element_js(reference: u32) -> String {
    wrap(&format!("const el = byRef({reference}); if (!el) return null; return JSON.stringify(describe(el, {reference}));"))
}

/// Fields of the form that contains element `ref` (passwords masked).
pub fn form_fields_js(reference: u32) -> String {
    wrap(&format!(
        r#"const el = byRef({reference}); if (!el) return null; const form = el.form || el.closest('form'); if (!form) return JSON.stringify([]);
  const out = [];
  for (const f of form.querySelectorAll('input,select,textarea')) {{
    const t = (f.getAttribute('type') || '').toLowerCase();
    if (['hidden', 'submit', 'button', 'image', 'reset'].includes(t)) continue;
    let v = f.tagName === 'SELECT' ? (f.selectedIndex >= 0 ? clean(f.options[f.selectedIndex].text) : '') : String(f.value || '');
    if (t === 'password') v = v ? '******' : '';
    if ((t === 'checkbox' || t === 'radio') && !f.checked) continue;
    out.push({{ label: label(f) || f.getAttribute('name') || '', name: f.getAttribute('name') || '', value: v.slice(0, 500) }});
  }}
  return JSON.stringify(out);"#
    ))
}

/// URL and payment markers of the current page, without re-tagging.
pub fn page_info_js() -> String {
    wrap(
        r#"return JSON.stringify({ url: location.href, title: document.title, text: '', elements: [], markers: {
    captcha: false, password: !!document.querySelector('input[type=password]'),
    card: !!document.querySelector('input[autocomplete^="cc-"],input[name*="card" i],input[id*="card" i],input[name*="cvv" i]') } });"#,
    )
}

/// One field of a form, as shown in a submission approval.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FormField {
    pub label: String,
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Element {
    #[serde(rename = "ref")]
    pub reference: u32,
    pub tag: String,
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub href: String,
    #[serde(default)]
    pub value: String,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub in_form: bool,
    #[serde(default)]
    pub form_action: String,
    #[serde(default)]
    pub form_method: String,
    #[serde(default)]
    pub autocomplete: String,
    #[serde(default)]
    pub expanded: bool,
    #[serde(default)]
    pub checked: bool,
    #[serde(default)]
    pub options: Vec<String>,
}

impl Element {
    pub fn is_password(&self) -> bool {
        self.r#type == "password" || self.autocomplete.contains("password")
    }

    pub fn is_text_input(&self) -> bool {
        (self.tag == "input" && matches!(self.r#type.as_str(), "" | "text" | "email" | "search" | "tel" | "url" | "number" | "password"))
            || self.tag == "textarea"
    }

    /// Human-readable name: visible text, else label, else name/id.
    pub fn display_name(&self) -> String {
        [&self.text, &self.label, &self.name, &self.id].iter().find(|s| !s.is_empty()).map(|s| s.to_string()).unwrap_or_default()
    }

    /// One line for the model.
    pub fn render(&self) -> String {
        let kind = match (self.tag.as_str(), self.r#type.as_str()) {
            ("a", _) => "链接".to_string(),
            ("input", t) if !t.is_empty() => format!("input[{t}]"),
            (t, _) => t.to_string(),
        };
        let mut s = format!("[{}] {kind} 「{}」", self.reference, self.display_name());
        if !self.label.is_empty() && self.label != self.display_name() {
            s.push_str(&format!(" 标签：{}", self.label));
        }
        if self.tag == "select" {
            s.push_str(&format!(" 选项：{}（当前：{}）", self.options.join("|"), self.value));
        } else if self.is_text_input() {
            if self.is_password() {
                s.push_str(" （密码框：hatch 不会填写）");
            } else {
                s.push_str(&format!(" 当前值=\"{}\"", self.value));
            }
        }
        if self.tag == "a" && !self.href.is_empty() {
            s.push_str(&format!(" → {}", self.href.chars().take(100).collect::<String>()));
        }
        if self.in_form && matches!(self.tag.as_str(), "button" | "input") && !self.is_text_input() {
            s.push_str(&format!(" （表单 {} {}）", self.form_method.to_uppercase(), self.form_action));
        }
        if self.disabled {
            s.push_str(" （不可用）");
        }
        s
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Markers {
    #[serde(default)]
    pub captcha: bool,
    #[serde(default)]
    pub password: bool,
    #[serde(default)]
    pub card: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    pub url: String,
    pub title: String,
    pub text: String,
    pub elements: Vec<Element>,
    #[serde(default)]
    pub markers: Markers,
}

impl Snapshot {
    pub fn parse(json: &str) -> Option<Snapshot> {
        serde_json::from_str(json).ok()
    }

    pub fn element(&self, reference: u32) -> Option<&Element> {
        self.elements.iter().find(|e| e.reference == reference)
    }

    /// Text for the model: title, URL, page text (capped) and the element list.
    pub fn render(&self, max_text: usize, max_elements: usize) -> String {
        let text: String = self.text.chars().take(max_text).collect();
        let more = self.text.chars().count().saturating_sub(max_text);
        let mut s = format!("标题：{}\n地址：{}\n\n正文：\n{text}", self.title, self.url);
        if more > 0 {
            s.push_str(&format!("…（还有 {more} 字，可用 browser_find 查找或 browser_scroll 后重新读取）"));
        }
        s.push_str("\n\n可操作元素（用编号引用）：\n");
        for e in self.elements.iter().take(max_elements) {
            s.push_str(&e.render());
            s.push('\n');
        }
        if self.elements.len() > max_elements {
            s.push_str(&format!("…（还有 {} 个元素）\n", self.elements.len() - max_elements));
        }
        if self.elements.is_empty() {
            s.push_str("（没有可操作元素）\n");
        }
        s
    }

    /// Lines of text and elements mentioning `query` (case-insensitive).
    pub fn find(&self, query: &str, limit: usize) -> Vec<String> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return Vec::new();
        }
        let mut out: Vec<String> = Vec::new();
        for seg in self.text.split(" ⏎ ") {
            if seg.to_lowercase().contains(&q) {
                out.push(format!("正文：{}", seg.chars().take(300).collect::<String>()));
            }
        }
        for e in &self.elements {
            let hay = format!("{} {} {} {} {}", e.text, e.label, e.name, e.href, e.options.join(" ")).to_lowercase();
            if hay.contains(&q) {
                out.push(format!("元素：{}", e.render()));
            }
        }
        out.truncate(limit);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Snapshot {
        Snapshot::parse(
            r#"{"url":"http://x/contact","title":"联系我们","text":"欢迎 ⏎ 请填写表单 ⏎ 我们会尽快回复",
            "elements":[
              {"ref":1,"tag":"input","type":"text","name":"name","label":"姓名","value":""},
              {"ref":2,"tag":"input","type":"password","name":"pw","label":"密码"},
              {"ref":3,"tag":"select","name":"city","options":["北京","上海"],"value":"北京"},
              {"ref":4,"tag":"button","type":"submit","text":"提交","in_form":true,"form_action":"/contact","form_method":"post"},
              {"ref":5,"tag":"a","href":"/about","text":"关于我们"}],
            "markers":{"captcha":false,"password":true,"card":false}}"#,
        )
        .unwrap()
    }

    #[test]
    fn render_and_find() {
        let s = sample();
        let r = s.render(100, 50);
        assert!(r.contains("[1] input[text] 「姓名」"), "{r}");
        assert!(r.contains("密码框：hatch 不会填写"));
        assert!(r.contains("选项：北京|上海（当前：北京）"));
        assert!(r.contains("（表单 POST /contact）"));
        assert!(r.contains("[5] 链接 「关于我们」 → /about"));
        assert!(s.element(2).unwrap().is_password());
        assert_eq!(s.find("上海", 5).len(), 1);
        assert_eq!(s.find("尽快", 5), vec!["正文：我们会尽快回复".to_string()]);
        assert!(s.find("", 5).is_empty());
        let long = Snapshot { text: "字".repeat(50), ..Default::default() };
        assert!(long.render(10, 5).contains("还有 40 字"));
    }

    #[test]
    fn scripts_are_fixed_expressions() {
        for js in [snapshot_js(), element_js(7), form_fields_js(3), page_info_js()] {
            assert!(js.starts_with("(() => {") && js.ends_with("})()"), "{js}");
        }
        assert!(snapshot_js().contains("data-hatch-ref"));
        assert!(element_js(7).contains("byRef(7)"));
        assert!(!element_js(7).contains("setAttribute"), "describing must not re-tag the page");
    }
}
