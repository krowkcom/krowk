//! Just enough of Go's non-strict `encoding/xml` to find a document's root
//! element and read its attributes: the prolog is skipped the way the decoder
//! skips it, and anything it would call a syntax error is no answer at all.

/// The root element's local name and attributes (local name, decoded value).
pub fn root_element(text: &str) -> Option<(String, Vec<(String, String)>)> {
    let mut rest = text;
    loop {
        rest = &rest[rest.find('<')?..];
        if let Some(pi) = rest.strip_prefix("<?") {
            let end = pi.find("?>")?;
            if !declaration_ok(&pi[..end]) {
                return None;
            }
            rest = &pi[end + 2..];
        } else if let Some(comment) = rest.strip_prefix("<!--") {
            rest = &comment[comment.find("-->")? + 3..];
        } else if let Some(directive) = rest.strip_prefix("<!") {
            rest = &directive[directive_end(directive)?..];
        } else if rest.starts_with("</") {
            // An end element with nothing open is an error even when not strict.
            return None;
        } else {
            return start_element(&rest[1..]);
        }
    }
}

/// An `<?xml ...?>` declaration naming a version or an encoding the decoder
/// cannot read — it has no CharsetReader — is an error there.
fn declaration_ok(pi: &str) -> bool {
    let (target, body) = pi.split_once(|c: char| c.is_whitespace()).unwrap_or((pi, ""));
    if target != "xml" {
        return true;
    }
    let (_, attrs) = attributes(body).unwrap_or_default();
    attrs.iter().all(|(k, v)| match k.as_str() {
        "version" => v == "1.0",
        "encoding" => v.is_empty() || v.eq_ignore_ascii_case("utf-8"),
        _ => true,
    })
}

/// Where a `<!...>` directive ends, past nested angle brackets and quotes.
fn directive_end(s: &str) -> Option<usize> {
    let (mut depth, mut quote) = (0, None);
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '<') => depth += 1,
            (None, '>') if depth == 0 => return Some(i + 1),
            (None, '>') => depth -= 1,
            _ => {}
        }
    }
    None
}

fn name_len(s: &str) -> usize {
    let mut chars = s.char_indices();
    match chars.next() {
        Some((_, c)) if c.is_alphabetic() || c == '_' || c == ':' => {}
        _ => return 0,
    }
    chars.find(|(_, c)| !(c.is_alphanumeric() || matches!(c, '_' | ':' | '.' | '-'))).map_or(s.len(), |(i, _)| i)
}

/// A name without its namespace prefix, as `xml.Name.Local` holds it.
fn local(name: &str) -> String {
    match name.split_once(':') {
        Some((space, local)) if !space.is_empty() && !local.is_empty() => local.to_owned(),
        _ => name.to_owned(),
    }
}

fn start_element(s: &str) -> Option<(String, Vec<(String, String)>)> {
    let n = name_len(s);
    if n == 0 {
        return None;
    }
    let (closed, attrs) = attributes(&s[n..])?;
    closed.then(|| (local(&s[..n]), attrs))
}

/// Attributes up to the end of a tag, non-strict: a bare name is its own value
/// and a value may go unquoted. The flag says whether the tag closed.
fn attributes(mut s: &str) -> Option<(bool, Vec<(String, String)>)> {
    let mut out = Vec::new();
    loop {
        s = s.trim_start();
        match s.chars().next() {
            None => return Some((false, out)),
            Some('>') => return Some((true, out)),
            Some('/') if s[1..].starts_with('>') => return Some((true, out)),
            Some('/') | Some('?') => return None,
            _ => {}
        }
        let n = name_len(s);
        if n == 0 {
            return None;
        }
        let name = local(&s[..n]);
        s = s[n..].trim_start();
        let Some(after) = s.strip_prefix('=') else {
            out.push((name.clone(), name));
            continue;
        };
        s = after.trim_start();
        let value = match s.chars().next() {
            Some(q @ ('"' | '\'')) => {
                let end = s[1..].find(q)?;
                let v = &s[1..end + 1];
                s = &s[end + 2..];
                v
            }
            _ => {
                let end = s.find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | ':' | '.' | '-'))).unwrap_or(s.len());
                if end == 0 {
                    return None;
                }
                let v = &s[..end];
                s = &s[end..];
                v
            }
        };
        out.push((name, entities(value)));
    }
}

fn entities(v: &str) -> String {
    v.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_root_is_found_past_the_prolog() {
        let doc = "<?xml version=\"1.0\"?>\n<!-- a -->\n<!DOCTYPE svg [<!ENTITY x \">\">]>\n<svg:svg width=\"12px\" height='8'>";
        let (name, attrs) = root_element(doc).unwrap();
        assert_eq!(name, "svg");
        assert_eq!(attrs, [("width".into(), "12px".into()), ("height".into(), "8".into())]);
        assert!(root_element("<?xml version=\"1.0\" encoding=\"latin1\"?><svg>").is_none());
        assert!(root_element("<svg width=\"1\"").is_none());
    }
}
