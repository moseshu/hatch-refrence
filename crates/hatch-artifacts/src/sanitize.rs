//! Allowlist HTML sanitiser for widget cards and HTML artifacts.
//!
//! See [`sanitize_html`] for the exact rules and SECURITY.md §9f for what
//! it does *not* guarantee.

// ---------------------------------------------------------------------------
// HTML sanitiser
// ---------------------------------------------------------------------------

/// Tags whose whole element — including its content — is dropped.
const DROP_CONTENT_TAGS: &[&str] = &[
    "script", "style", "iframe", "object", "embed", "form", "noscript", "template", "textarea", "select", "button", "input",
    "meta", "link", "base", "frame", "frameset", "title", "svg", "math", "video", "audio", "canvas",
];

/// Tags kept (content kept for tags not in either list, tag itself dropped).
const ALLOWED_TAGS: &[&str] = &[
    "p", "h1", "h2", "h3", "h4", "h5", "h6", "ul", "ol", "li", "strong", "b", "em", "i", "a", "code", "pre", "blockquote",
    "table", "thead", "tbody", "tr", "th", "td", "br", "hr", "div", "span", "img",
];

/// Sanitise HTML with an allowlist.
///
/// Rules:
/// * Only the tags in `ALLOWED_TAGS` survive; anything else loses its
///   tag (content kept) except `DROP_CONTENT_TAGS`, whose content is
///   dropped too (`script`, `style`, `iframe`, `object`, `embed`,
///   `form`, `svg`, `video`, `audio`, …).
/// * Comments, `<!…>` declarations and `<?…?>` processing instructions
///   are dropped. Text nodes are HTML-escaped, so a stray `<` can never
///   start a tag the scanner missed.
/// * Attributes: only `href` on `<a>`, `src`/`alt` on `<img>`, and
///   `title` anywhere survive. Every `on*` handler, `style`, `class`,
///   `id` and any other attribute is stripped.
/// * URLs: `javascript:`, `data:`, `vbscript:` schemes, any other scheme
///   (`http:`/`https:` included) and protocol-relative `//…` URLs are
///   stripped — a card may not fetch anything external. Relative links
///   and `#fragment` links survive.
/// * The sanitiser is a scanner, not a full HTML parser: malformed input
///   degrades to escaped text, never to a live tag.
///
/// What it does NOT guarantee: see SECURITY.md §9f (it does not make
/// arbitrary HTML safe to render in a privileged context, and it does
/// not vouch for where a surviving relative link points).
pub fn sanitize_html(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            // Text node: escape until the next '<'.
            let start = i;
            while i < bytes.len() && bytes[i] != b'<' {
                i += 1;
            }
            escape_text_into(&input[start..i], &mut out);
            continue;
        }
        // A '<' only starts a tag when followed by a letter, '/', '!'
        // or '?'. Otherwise (e.g. "1 < 2") it is literal text.
        let next = bytes.get(i + 1).copied().unwrap_or(b' ');
        if !(next.is_ascii_alphabetic() || next == b'/' || next == b'!' || next == b'?') {
            out.push_str("&lt;");
            i += 1;
            continue;
        }
        // Comment.
        if input[i..].starts_with("<!--") {
            match input[i + 4..].find("-->") {
                Some(end) => i += 4 + end + 3,
                None => break, // Unterminated comment: drop the rest.
            }
            continue;
        }
        // Declaration / processing instruction: drop to the next '>'.
        if input[i..].starts_with("<!") || input[i..].starts_with("<?") {
            match input[i..].find('>') {
                Some(end) => i += end + 1,
                None => break,
            }
            continue;
        }
        // Find the end of this tag, honouring quotes.
        let mut j = i + 1;
        let mut quote: Option<u8> = None;
        while j < bytes.len() {
            match (quote, bytes[j]) {
                (None, b'>') => break,
                (None, b'"') | (None, b'\'') => quote = Some(bytes[j]),
                (Some(q), b) if b == q => quote = None,
                _ => {}
            }
            j += 1;
        }
        if j >= bytes.len() {
            // No closing '>': treat the '<' as text.
            out.push_str("&lt;");
            i += 1;
            continue;
        }
        let tag_inner = &input[i + 1..j];
        i = j + 1;
        let (closing, name, attrs) = parse_tag(tag_inner);
        let name_lc = name.to_ascii_lowercase();
        if DROP_CONTENT_TAGS.contains(&name_lc.as_str()) {
            if !closing {
                skip_element_content(input, &mut i, &name_lc);
            }
            continue;
        }
        if !ALLOWED_TAGS.contains(&name_lc.as_str()) {
            continue; // Drop the tag, keep its content.
        }
        if closing {
            out.push_str(&format!("</{name_lc}>"));
            continue;
        }
        out.push('<');
        out.push_str(&name_lc);
        for (aname, avalue) in attrs {
            let aname_lc = aname.to_ascii_lowercase();
            let keep = match name_lc.as_str() {
                "a" => aname_lc == "href" || aname_lc == "title",
                "img" => aname_lc == "src" || aname_lc == "alt" || aname_lc == "title",
                _ => aname_lc == "title",
            };
            if !keep {
                continue;
            }
            if (aname_lc == "href" || aname_lc == "src") && !safe_url(&avalue) {
                continue;
            }
            out.push(' ');
            out.push_str(&aname_lc);
            out.push_str("=\"");
            escape_attr_into(&avalue, &mut out);
            out.push('"');
        }
        out.push('>');
    }
    out
}

fn escape_text_into(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
}

fn escape_attr_into(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
}

/// Split a tag's inner text into (is_closing, name, attributes).
fn parse_tag(inner: &str) -> (bool, String, Vec<(String, String)>) {
    let inner = inner.trim();
    let (closing, rest) = match inner.strip_prefix('/') {
        Some(r) => (true, r),
        None => (false, inner),
    };
    let mut chars = rest.char_indices().peekable();
    let mut name = String::new();
    while let Some((_, c)) = chars.peek() {
        if c.is_ascii_alphanumeric() || *c == '-' {
            name.push(*c);
            chars.next();
        } else {
            break;
        }
    }
    let mut attrs = Vec::new();
    let rest: String = chars.map(|(_, c)| c).collect();
    let rb = rest.as_bytes();
    let mut k = 0;
    while k < rb.len() {
        while k < rb.len() && (rb[k] as char).is_whitespace() {
            k += 1;
        }
        if k >= rb.len() || rb[k] == b'/' {
            break;
        }
        let astart = k;
        while k < rb.len() && !((rb[k] as char).is_whitespace() || rb[k] == b'=' || rb[k] == b'/') {
            k += 1;
        }
        if k == astart {
            break;
        }
        let aname = rest[astart..k].to_string();
        while k < rb.len() && (rb[k] as char).is_whitespace() {
            k += 1;
        }
        let mut avalue = String::new();
        if k < rb.len() && rb[k] == b'=' {
            k += 1;
            while k < rb.len() && (rb[k] as char).is_whitespace() {
                k += 1;
            }
            if k < rb.len() && (rb[k] == b'"' || rb[k] == b'\'') {
                let q = rb[k];
                k += 1;
                let vstart = k;
                while k < rb.len() && rb[k] != q {
                    k += 1;
                }
                avalue = rest[vstart..k].to_string();
                if k < rb.len() {
                    k += 1;
                }
            } else {
                let vstart = k;
                while k < rb.len() && !((rb[k] as char).is_whitespace() || rb[k] == b'/') {
                    k += 1;
                }
                avalue = rest[vstart..k].to_string();
            }
        }
        attrs.push((aname, avalue));
    }
    (closing, name, attrs)
}

/// Skip element content up to (and including) the matching close tag,
/// counting nesting of the same tag name.
fn skip_element_content(input: &str, i: &mut usize, name: &str) {
    let lower = input.to_ascii_lowercase();
    let open = format!("<{name}");
    let close = format!("</{name}");
    let mut depth = 1usize;
    let mut pos = *i;
    while depth > 0 {
        let next_open = lower[pos..].find(&open).map(|p| pos + p);
        let next_close = lower[pos..].find(&close).map(|p| pos + p);
        match (next_open, next_close) {
            (Some(o), Some(c)) if o < c => {
                depth += 1;
                pos = o + open.len();
            }
            (_, Some(c)) => {
                depth -= 1;
                pos = c + close.len();
                if let Some(end) = input[pos..].find('>') {
                    pos += end + 1;
                }
            }
            _ => {
                pos = input.len();
                break;
            }
        }
    }
    *i = pos;
}

/// True when a URL may survive sanitisation: relative or `#fragment`
/// only — no scheme, no protocol-relative URL.
fn safe_url(u: &str) -> bool {
    let t = u.trim().to_ascii_lowercase();
    if t.is_empty() || t.starts_with('#') {
        return true;
    }
    if t.starts_with("//") {
        return false;
    }
    // Any ':' before the first '/', '?' or '#' is a scheme.
    for (idx, c) in t.char_indices() {
        match c {
            ':' => return false,
            '/' | '?' | '#' => break,
            _ => {
                let _ = idx;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitiser_strips_script_event_handlers_and_external_fetches() {
        let dirty = r##"<p onclick="steal()">你好</p><script>alert(1)</script><style>p{color:red}</style><a href="javascript:alert(1)">x</a><a href="https://evil.example/x">y</a><img src="https://evil.example/t.png" onerror="x()"><iframe src="https://evil.example"></iframe><a href="#sec">内链</a><a href="doc.md">相对</a><b>粗体</b>"##;
        let clean = sanitize_html(dirty);
        assert!(!clean.contains("<script") && !clean.contains("alert(1)"), "{clean}");
        assert!(!clean.contains("onclick") && !clean.contains("onerror"), "{clean}");
        assert!(!clean.contains("javascript:") && !clean.contains("evil.example"), "{clean}");
        assert!(!clean.contains("<style") && !clean.contains("color:red"), "{clean}");
        assert!(!clean.contains("<iframe"), "{clean}");
        assert!(clean.contains("你好"), "{clean}");
        assert!(clean.contains("<b>粗体</b>"), "{clean}");
        assert!(clean.contains("href=\"#sec\""), "{clean}");
        assert!(clean.contains("href=\"doc.md\""), "{clean}");
    }

    #[test]
    fn sanitiser_keeps_safe_markup_and_escapes_text() {
        let clean = sanitize_html("<h1>标题</h1><p>1 < 2 &amp; 3 > 2</p><ul><li>一项</li></ul>");
        assert!(clean.contains("<h1>标题</h1>"), "{clean}");
        assert!(clean.contains("1 &lt; 2"), "{clean}");
        assert!(clean.contains("<li>一项</li>"), "{clean}");
        // Unknown tag dropped, content kept.
        assert_eq!(sanitize_html("<section>内容</section>"), "内容");
        // Unclosed tag degrades to escaped text, never a live tag.
        assert!(!sanitize_html("<p 未闭合").contains("<p "));
    }
}
