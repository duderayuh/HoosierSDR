//! A deliberately small, tolerant XML reader for SOAP responses.
//!
//! Hand-rolled for the same reason `csv` is: the input is machine-generated
//! and structurally simple, and keeping it in-tree means the RadioReference
//! feature pulls in an HTTP client and nothing else.
//!
//! "Tolerant" is the important word, and it is a deliberate response to a
//! constraint: the exact element names the service returns cannot be verified
//! from this repository's build environment, and SOAP responses vary in
//! namespace prefixing and letter case between service versions. So lookups
//! match on the **local name, case-insensitively** — `<ns2:siteFreq>`,
//! `<SiteFreq>` and `<sitefreq>` are all the same field — and unrecognized
//! elements are preserved rather than dropped, so a response whose field names
//! differ from what the mapping expects can be dumped and inspected instead of
//! silently decoding to nothing.

/// A parsed XML element: local name, attributes, text, and children.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Node {
    /// Element name with any namespace prefix stripped.
    pub name: String,
    pub attrs: Vec<(String, String)>,
    /// Concatenated direct text content, trimmed.
    pub text: String,
    pub children: Vec<Node>,
}

impl Node {
    /// Direct children whose local name matches `name`, case-insensitively.
    pub fn all(&self, name: &str) -> impl Iterator<Item = &Node> {
        let want = name.to_ascii_lowercase();
        self.children
            .iter()
            .filter(move |c| c.name.eq_ignore_ascii_case(&want))
    }

    /// First direct child with this local name.
    pub fn child(&self, name: &str) -> Option<&Node> {
        self.all(name).next()
    }

    /// Find the first descendant (depth-first, self included) with this name.
    /// SOAP nests payloads inside Envelope/Body/Response wrappers whose exact
    /// naming varies, so reaching in by name beats hard-coding a path.
    pub fn find(&self, name: &str) -> Option<&Node> {
        if self.name.eq_ignore_ascii_case(name) {
            return Some(self);
        }
        self.children.iter().find_map(|c| c.find(name))
    }

    /// All descendants with this name (depth-first).
    pub fn find_all<'a>(&'a self, name: &str, out: &mut Vec<&'a Node>) {
        if self.name.eq_ignore_ascii_case(name) {
            out.push(self);
        }
        for c in &self.children {
            c.find_all(name, out);
        }
    }

    /// Text of the first child with this name.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.child(name)
            .map(|c| c.text.as_str())
            .filter(|t| !t.is_empty())
    }

    /// Text of the first child with any of these names — for fields whose
    /// spelling differs across service versions.
    pub fn get_any(&self, names: &[&str]) -> Option<&str> {
        names.iter().find_map(|n| self.get(n))
    }

    pub fn get_u64(&self, name: &str) -> Option<u64> {
        self.get(name)?.trim().parse().ok()
    }

    pub fn get_f64_any(&self, names: &[&str]) -> Option<f64> {
        self.get_any(names)?.trim().parse().ok()
    }

    pub fn get_u64_any(&self, names: &[&str]) -> Option<u64> {
        self.get_any(names)?.trim().parse().ok()
    }
}

/// Parse an XML document into its root element.
pub fn parse(src: &str) -> Option<Node> {
    let b = src.as_bytes();
    let mut i = 0usize;
    let mut stack: Vec<Node> = Vec::new();
    let mut root: Option<Node> = None;

    while i < b.len() {
        if b[i] != b'<' {
            // Text run up to the next tag.
            let start = i;
            while i < b.len() && b[i] != b'<' {
                i += 1;
            }
            if let Some(top) = stack.last_mut() {
                let t = decode_entities(&src[start..i]);
                let t = t.trim();
                if !t.is_empty() {
                    if !top.text.is_empty() {
                        top.text.push(' ');
                    }
                    top.text.push_str(t);
                }
            }
            continue;
        }
        // A tag of some kind.
        if src[i..].starts_with("<!--") {
            i = find_from(src, i, "-->").map(|p| p + 3)?;
            continue;
        }
        if src[i..].starts_with("<![CDATA[") {
            let end = find_from(src, i, "]]>")?;
            if let Some(top) = stack.last_mut() {
                top.text.push_str(&src[i + 9..end]);
            }
            i = end + 3;
            continue;
        }
        if src[i..].starts_with("<?") || src[i..].starts_with("<!") {
            i = find_from(src, i, ">").map(|p| p + 1)?;
            continue;
        }
        let end = find_from(src, i, ">")?;
        let inner = &src[i + 1..end];
        i = end + 1;

        if let Some(close) = inner.strip_prefix('/') {
            // Closing tag: pop, tolerating mismatches rather than failing.
            let name = local_name(close.trim());
            let popped = pop_to(&mut stack, &name);
            if let Some(node) = popped {
                match stack.last_mut() {
                    Some(parent) => parent.children.push(node),
                    None => root = Some(node),
                }
            }
            continue;
        }

        let self_closing = inner.ends_with('/');
        let body = inner.strip_suffix('/').unwrap_or(inner).trim();
        if body.is_empty() {
            continue;
        }
        let (raw_name, rest) = split_once_ws(body);
        let node = Node {
            name: local_name(raw_name),
            attrs: parse_attrs(rest),
            ..Default::default()
        };
        if self_closing {
            match stack.last_mut() {
                Some(parent) => parent.children.push(node),
                None => root = Some(node),
            }
        } else {
            stack.push(node);
        }
    }

    // Unclosed elements at EOF: keep what we have rather than losing the parse.
    while let Some(node) = stack.pop() {
        match stack.last_mut() {
            Some(parent) => parent.children.push(node),
            None => root = Some(node),
        }
    }
    root
}

/// Pop the stack down to the element matching `name`, returning it. If nothing
/// matches, pop just the top (a stray close tag shouldn't unwind the document).
fn pop_to(stack: &mut Vec<Node>, name: &str) -> Option<Node> {
    let hit = stack
        .iter()
        .rposition(|n| n.name.eq_ignore_ascii_case(name));
    match hit {
        Some(idx) => {
            // Fold any unclosed inner elements into their parents.
            while stack.len() > idx + 1 {
                let node = stack.pop()?;
                stack.last_mut()?.children.push(node);
            }
            stack.pop()
        }
        None => stack.pop(),
    }
}

fn local_name(raw: &str) -> String {
    let n = raw.rsplit(':').next().unwrap_or(raw);
    n.trim().to_string()
}

fn split_once_ws(s: &str) -> (&str, &str) {
    match s.find(char::is_whitespace) {
        Some(p) => (&s[..p], s[p..].trim_start()),
        None => (s, ""),
    }
}

fn find_from(s: &str, from: usize, pat: &str) -> Option<usize> {
    s[from..].find(pat).map(|p| p + from)
}

fn parse_attrs(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && (b[i] as char).is_whitespace() {
            i += 1;
        }
        let start = i;
        while i < b.len() && b[i] != b'=' && !(b[i] as char).is_whitespace() {
            i += 1;
        }
        if start == i {
            break;
        }
        let key = local_name(&s[start..i]);
        while i < b.len() && (b[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= b.len() || b[i] != b'=' {
            out.push((key, String::new()));
            continue;
        }
        i += 1;
        while i < b.len() && (b[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let quote = b[i];
        let val = if quote == b'"' || quote == b'\'' {
            i += 1;
            let vs = i;
            while i < b.len() && b[i] != quote {
                i += 1;
            }
            let v = &s[vs..i];
            i += 1;
            v
        } else {
            let vs = i;
            while i < b.len() && !(b[i] as char).is_whitespace() {
                i += 1;
            }
            &s[vs..i]
        };
        out.push((key, decode_entities(val)));
    }
    out
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        rest = &rest[p..];
        let Some(semi) = rest.find(';').filter(|&e| e <= 12) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..semi];
        let decoded = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => ent
                .strip_prefix('#')
                .and_then(
                    |n| match n.strip_prefix('x').or_else(|| n.strip_prefix('X')) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok(),
                        None => n.parse::<u32>().ok(),
                    },
                )
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_nested_soap_shaped_document() {
        let doc = r#"<?xml version="1.0"?>
        <SOAP-ENV:Envelope xmlns:SOAP-ENV="http://schemas.xmlsoap.org/soap/envelope/">
          <SOAP-ENV:Body>
            <ns1:getTrsSitesResponse>
              <return>
                <item><siteId>1</siteId><nac>0x261</nac></item>
                <item><siteId>2</siteId><nac>0x1AB</nac></item>
              </return>
            </ns1:getTrsSitesResponse>
          </SOAP-ENV:Body>
        </SOAP-ENV:Envelope>"#;
        let root = parse(doc).expect("parses");
        assert_eq!(root.name, "Envelope");
        let ret = root
            .find("return")
            .expect("finds return through namespaces");
        let items: Vec<_> = ret.all("item").collect();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].get("siteId"), Some("1"));
        assert_eq!(items[1].get("nac"), Some("0x1AB"));
    }

    #[test]
    fn field_lookup_ignores_case_and_namespace() {
        let root = parse("<a><ns:SiteFreq>851.0125</ns:SiteFreq></a>").unwrap();
        assert_eq!(root.get("sitefreq"), Some("851.0125"));
        assert_eq!(root.get("SITEFREQ"), Some("851.0125"));
        assert_eq!(root.get_f64_any(&["freq", "siteFreq"]), Some(851.0125));
    }

    #[test]
    fn decodes_entities_in_text_and_attributes() {
        let root =
            parse(r#"<a t="Fire &amp; EMS"><n>Sheriff &lt;North&gt; &#65;</n></a>"#).unwrap();
        assert_eq!(root.get("n"), Some("Sheriff <North> A"));
        assert_eq!(root.attrs[0].1, "Fire & EMS");
    }

    #[test]
    fn handles_self_closing_and_cdata() {
        let root = parse("<a><empty/><d><![CDATA[raw <not a tag>]]></d></a>").unwrap();
        assert!(root.child("empty").is_some());
        assert_eq!(root.get("d"), Some("raw <not a tag>"));
    }

    #[test]
    fn a_truncated_document_still_yields_what_was_read() {
        // A connection cut mid-response must not lose the records already
        // received, so partial results stay diagnosable.
        let root = parse("<r><item><id>7</id></item><item><id>8</id>").unwrap();
        let items: Vec<_> = root.all("item").collect();
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].get("id"), Some("8"));
    }
}
