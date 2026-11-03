//! Very small HTML → text conversion: enough for a model to read an article,
//! not a browser. Scripts, styles and comments are dropped entirely.

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let end = tail.find(';').filter(|e| *e <= 10);
        let decoded = end.and_then(|e| {
            let name = &tail[1..e];
            let c = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" | "#39" => Some('\''),
                "nbsp" => Some(' '),
                _ if name.starts_with("#x") || name.starts_with("#X") => u32::from_str_radix(&name[2..], 16).ok().and_then(char::from_u32),
                _ if name.starts_with('#') => name[1..].parse().ok().and_then(char::from_u32),
                _ => None,
            };
            c.map(|c| (c, e))
        });
        match decoded {
            Some((c, e)) => {
                out.push(c);
                rest = &tail[e + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Remove `<tag ...> ... </tag>` blocks (case-insensitive).
fn drop_blocks(html: &str, tag: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let (open, close) = (format!("<{tag}"), format!("</{tag}"));
    let mut out = String::with_capacity(html.len());
    let mut pos = 0;
    while let Some(s) = lower[pos..].find(&open).map(|i| i + pos) {
        out.push_str(&html[pos..s]);
        match lower[s..].find(&close).map(|i| i + s) {
            Some(e) => pos = lower[e..].find('>').map(|i| i + e + 1).unwrap_or(html.len()),
            None => {
                pos = html.len();
                break;
            }
        }
    }
    out.push_str(&html[pos.min(html.len())..]);
    out
}

const BLOCK_TAGS: &[&str] = &["p", "div", "br", "li", "tr", "h1", "h2", "h3", "h4", "h5", "h6", "section", "article", "pre", "table", "ul", "ol", "header", "footer"];

pub fn to_text(html: &str) -> String {
    let mut s = html.to_string();
    for tag in ["script", "style", "noscript", "svg", "template", "iframe"] {
        s = drop_blocks(&s, tag);
    }
    // Comments.
    while let Some(i) = s.find("<!--") {
        match s[i..].find("-->") {
            Some(e) => s.replace_range(i..i + e + 3, " "),
            None => s.truncate(i),
        }
    }
    let mut text = String::with_capacity(s.len());
    let mut in_tag = false;
    let mut tag = String::new();
    for ch in s.chars() {
        match (in_tag, ch) {
            (false, '<') => {
                in_tag = true;
                tag.clear();
            }
            (true, '>') => {
                in_tag = false;
                let name: String = tag.trim_start_matches('/').chars().take_while(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase();
                if BLOCK_TAGS.contains(&name.as_str()) {
                    text.push('\n');
                }
            }
            (true, c) => tag.push(c),
            (false, c) => text.push(c),
        }
    }
    let decoded = decode_entities(&text);
    let mut out = String::new();
    let mut blank = 0;
    for line in decoded.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            blank += 1;
            if blank == 1 && !out.is_empty() {
                out.push('\n');
            }
        } else {
            blank = 0;
            out.push_str(&line);
            out.push('\n');
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_scripts_tags_and_decodes() {
        let html = "<html><head><style>p{}</style><script>alert('x')</script></head><body><h1>标题 &amp; 副标题</h1><!-- hidden --><p>第一段&nbsp;文字</p><p>a &lt; b &#x4e2d;</p></body></html>";
        let t = to_text(html);
        assert!(t.contains("标题 & 副标题"));
        assert!(t.contains("第一段 文字"));
        assert!(t.contains("a < b 中"));
        assert!(!t.contains("alert") && !t.contains("hidden") && !t.contains("p{}"));
        assert_eq!(decode_entities("AT&T &bogus; &#65;"), "AT&T &bogus; A");
    }
}
