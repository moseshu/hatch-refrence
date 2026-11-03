//! Minimal XML reader for CalDAV multistatus responses.
//!
//! Hand-rolled on purpose: the workspace has no XML crate, and CalDAV
//! only needs a handful of elements (`href`, `getetag`, `sync-token`,
//! `calendar-data`, `status`, `response`). The parser builds a tiny
//! DOM, decodes entities and CDATA, ignores namespaces (matching is on
//! the local name after the `:`), and **fails loudly** on malformed
//! input — a truncated or mismatched response must never be mistaken
//! for "no changes".
//!
//! Deliberately not a general XML parser: no DTD validation, no
//! external entities (never fetched), attribute values are kept raw
//! except for entity decoding.

use hatch_core::{Error, Result};

/// One element: name (with any namespace prefix kept), attributes,
/// child elements, and concatenated text content (entity-decoded).
#[derive(Clone, Debug, PartialEq)]
pub struct XmlNode {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<XmlNode>,
    pub text: String,
}

impl XmlNode {
    /// Local name: `DAV:response` → `response`, `response` → `response`.
    pub fn local(&self) -> &str {
        self.name.rsplit(':').next().unwrap_or(&self.name)
    }

    /// First direct child with this local name.
    pub fn child(&self, local: &str) -> Option<&XmlNode> {
        self.children.iter().find(|c| c.local() == local)
    }

    /// Text of the first direct child with this local name.
    pub fn child_text(&self, local: &str) -> Option<String> {
        self.child(local).map(|c| c.text.clone())
    }

    /// All descendants (including self) with this local name, in order.
    pub fn descendants(&self, local: &str) -> Vec<&XmlNode> {
        let mut out = Vec::new();
        self.collect(local, &mut out);
        out
    }

    fn collect<'a>(&'a self, local: &str, out: &mut Vec<&'a XmlNode>) {
        if self.local() == local {
            out.push(self);
        }
        for c in &self.children {
            c.collect(local, out);
        }
    }

    /// First descendant with this local name (depth-first).
    pub fn descendant(&self, local: &str) -> Option<&XmlNode> {
        self.descendants(local).into_iter().next()
    }
}

fn decode_entities(s: &str) -> Result<String> {
    if !s.contains('&') {
        return Ok(s.to_string());
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let Some(semi) = after.find(';') else {
            return Err(Error::Invalid("unterminated XML entity".into()));
        };
        let ent = &after[..semi];
        match ent {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            _ if let Some(num) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")) => {
                let cp = u32::from_str_radix(num, 16)
                    .ok()
                    .and_then(char::from_u32)
                    .ok_or_else(|| Error::Invalid(format!("bad character reference &{ent};")))?;
                out.push(cp);
            }
            _ if let Some(num) = ent.strip_prefix('#') => {
                let cp = num
                    .parse::<u32>()
                    .ok()
                    .and_then(char::from_u32)
                    .ok_or_else(|| Error::Invalid(format!("bad character reference &{ent};")))?;
                out.push(cp);
            }
            _ => return Err(Error::Invalid(format!("unknown XML entity &{ent};"))),
        }
        rest = &after[semi + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

struct Parser<'a> {
    s: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&'a str> {
        self.s.get(self.pos..)
    }

    fn starts_with(&self, pat: &str) -> bool {
        self.peek().map(|r| r.starts_with(pat)).unwrap_or(false)
    }

    fn bump(&mut self, n: usize) {
        self.pos += n;
    }

    /// Skip whitespace, processing instructions, comments and DOCTYPE.
    fn skip_misc(&mut self) -> Result<()> {
        loop {
            let Some(r) = self.peek() else { return Ok(()) };
            if r.starts_with(|c: char| c.is_whitespace()) {
                self.bump(r.chars().next().unwrap().len_utf8());
            } else if r.starts_with("<?") {
                let Some(end) = r.find("?>") else {
                    return Err(Error::Invalid("unterminated processing instruction".into()));
                };
                self.bump(end + 2);
            } else if r.starts_with("<!--") {
                let Some(end) = r.find("-->") else {
                    return Err(Error::Invalid("unterminated XML comment".into()));
                };
                self.bump(end + 3);
            } else if r.starts_with("<!DOCTYPE") || r.starts_with("<!doctype") {
                // Skip to `>`, respecting an internal `[...]` subset.
                let mut depth = 0usize;
                let mut i = 0usize;
                let bytes = r.as_bytes();
                let mut found = None;
                while i < bytes.len() {
                    match bytes[i] {
                        b'[' => depth += 1,
                        b']' => depth = depth.saturating_sub(1),
                        b'>' if depth == 0 => {
                            found = Some(i);
                            break;
                        }
                        _ => {}
                    }
                    i += 1;
                }
                let Some(end) = found else {
                    return Err(Error::Invalid("unterminated DOCTYPE".into()));
                };
                self.bump(end + 1);
            } else {
                return Ok(());
            }
        }
    }

    fn parse_name(&mut self) -> Result<String> {
        let r = self.peek().unwrap_or("");
        let mut end = 0usize;
        for c in r.chars() {
            if c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ':') {
                end += c.len_utf8();
            } else {
                break;
            }
        }
        if end == 0 {
            return Err(Error::Invalid(format!(
                "expected element name at byte {pos}",
                pos = self.pos
            )));
        }
        let name = r[..end].to_string();
        self.bump(end);
        Ok(name)
    }

    fn parse_attrs(&mut self) -> Result<Vec<(String, String)>> {
        let mut attrs = Vec::new();
        loop {
            self.skip_ws();
            let Some(r) = self.peek() else {
                return Err(Error::Invalid("unterminated start tag".into()));
            };
            if r.starts_with('>') || r.starts_with("/>") {
                return Ok(attrs);
            }
            let name = self.parse_name()?;
            self.skip_ws();
            if !self.starts_with("=") {
                return Err(Error::Invalid(format!("attribute {name} has no value")));
            }
            self.bump(1);
            self.skip_ws();
            let r = self.peek().unwrap_or("");
            let quote = r.chars().next().unwrap_or(' ');
            if quote != '"' && quote != '\'' {
                return Err(Error::Invalid(format!("attribute {name} value is not quoted")));
            }
            self.bump(1);
            let r = self.peek().unwrap_or("");
            let Some(end) = r.find(quote) else {
                return Err(Error::Invalid(format!("unterminated attribute {name}")));
            };
            let raw = &r[..end];
            attrs.push((name, decode_entities(raw)?));
            self.bump(end + 1);
        }
    }

    fn skip_ws(&mut self) {
        while let Some(r) = self.peek() {
            if r.starts_with(|c: char| c.is_whitespace()) {
                self.bump(r.chars().next().unwrap().len_utf8());
            } else {
                break;
            }
        }
    }

    fn parse_element(&mut self) -> Result<XmlNode> {
        self.skip_misc()?;
        if !self.starts_with("<") || self.starts_with("</") {
            return Err(Error::Invalid(format!(
                "expected element at byte {pos}",
                pos = self.pos
            )));
        }
        self.bump(1);
        let name = self.parse_name()?;
        let attrs = self.parse_attrs()?;
        if self.starts_with("/>") {
            self.bump(2);
            return Ok(XmlNode { name, attrs, children: Vec::new(), text: String::new() });
        }
        if !self.starts_with(">") {
            return Err(Error::Invalid(format!("bad start tag <{name}>")));
        }
        self.bump(1);
        let mut node = XmlNode { name: name.clone(), attrs, children: Vec::new(), text: String::new() };
        loop {
            let Some(r) = self.peek() else {
                return Err(Error::Invalid(format!("unclosed element <{name}>")));
            };
            if r.is_empty() {
                return Err(Error::Invalid(format!("unclosed element <{name}>")));
            }
            if r.starts_with("</") {
                self.bump(2);
                let close = self.parse_name()?;
                self.skip_ws();
                if !self.starts_with(">") {
                    return Err(Error::Invalid(format!("bad end tag </{close}>")));
                }
                self.bump(1);
                if close != name {
                    return Err(Error::Invalid(format!(
                        "mismatched tags: <{name}> closed by </{close}>"
                    )));
                }
                return Ok(node);
            }
            if r.starts_with("<![CDATA[") {
                let Some(end) = r.find("]]>") else {
                    return Err(Error::Invalid("unterminated CDATA section".into()));
                };
                node.text.push_str(&r[9..end]);
                self.bump(end + 3);
                continue;
            }
            if r.starts_with("<!--") || r.starts_with("<?") {
                self.skip_misc()?;
                continue;
            }
            if r.starts_with('<') {
                let child = self.parse_element()?;
                node.children.push(child);
                continue;
            }
            // Text up to the next `<`.
            let end = r.find('<').unwrap_or(r.len());
            node.text.push_str(&decode_entities(&r[..end])?);
            self.bump(end);
        }
    }
}

/// Parse one XML document into a DOM. Errors on anything malformed:
/// junk after the root element, unclosed or mismatched tags, bad
/// entities. A document with no root element is an error.
pub fn parse_xml(input: &str) -> Result<XmlNode> {
    let mut p = Parser { s: input, pos: 0 };
    p.skip_misc()?;
    let root = p.parse_element()?;
    p.skip_misc()?;
    if p.peek().is_some_and(|r| !r.is_empty()) {
        return Err(Error::Invalid("junk after the XML root element".into()));
    }
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MULTI: &str = r#"<?xml version="1.0" encoding="utf-8" ?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/cal/events/u1.ics</d:href>
    <d:propstat><d:prop>
      <d:getetag>"abc-1"</d:getetag>
      <c:calendar-data>BEGIN:VCALENDAR</c:calendar-data>
    </d:prop></d:propstat>
  </d:response>
  <d:response>
    <d:href>/cal/events/u2.ics</d:href>
    <d:status>HTTP/1.1 404 Not Found</d:status>
  </d:response>
  <d:sync-token>http://example.com/sync/42</d:sync-token>
</d:multistatus>"#;

    #[test]
    fn parses_multistatus_by_local_name() {
        let root = parse_xml(MULTI).unwrap();
        assert_eq!(root.local(), "multistatus");
        let responses = root.descendants("response");
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0].child_text("href").unwrap(), "/cal/events/u1.ics");
        assert_eq!(responses[0].descendant("getetag").unwrap().text, "\"abc-1\"");
        assert_eq!(responses[0].descendant("calendar-data").unwrap().text, "BEGIN:VCALENDAR");
        assert_eq!(responses[1].child_text("status").unwrap(), "HTTP/1.1 404 Not Found");
        assert_eq!(root.child_text("sync-token").unwrap(), "http://example.com/sync/42");
    }

    #[test]
    fn entities_cdata_comments_and_self_closing() {
        let root = parse_xml(
            "<r><!-- hi --><a x=\"1 &amp; 2\"/><b><![CDATA[a<b>&amp;c]]></b><c>Tom &lt; Jerry &quot;x&quot; &#65;</c></r>",
        )
        .unwrap();
        assert_eq!(root.child("a").unwrap().attrs, vec![("x".to_string(), "1 & 2".to_string())]);
        assert_eq!(root.child_text("b").unwrap(), "a<b>&amp;c");
        assert_eq!(root.child_text("c").unwrap(), "Tom < Jerry \"x\" A");
    }

    #[test]
    fn malformed_xml_is_an_error_not_empty_data() {
        for bad in [
            "",
            "not xml at all",
            "<r><a></r>",
            "<r><a>",
            "<r>junk</r> trailing",
            "<r>&bogus;</r>",
            "<r>&#xZZ;</r>",
            "<r><a b=1/></r>",
            "<r><!-- unclosed</r>",
        ] {
            assert!(parse_xml(bad).is_err(), "{bad:?} should fail");
        }
    }
}
