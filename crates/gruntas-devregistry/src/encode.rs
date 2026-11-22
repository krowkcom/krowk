//! Responses written the way `json.Encoder` with `SetIndent("", "  ")` writes
//! them — the compact form, re-indented — plus the canonical form an
//! Idempotency-Key digest is taken over.

use crate::json::Value;

/// A response value. `Obj` is written in the order given; `map` sorts, as Go
/// sorts a map's keys. `Raw` is JSON text already compacted.
#[derive(Debug, Clone)]
pub enum Json {
    Null,
    Int(i64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
    Raw(String),
}

impl Json {
    pub fn map<const N: usize>(pairs: [(&str, Json); N]) -> Json {
        Json::map_of(pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
    }

    pub fn map_of(mut pairs: Vec<(String, Json)>) -> Json {
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        Json::Obj(pairs)
    }

    pub fn str(s: impl Into<String>) -> Json {
        Json::Str(s.into())
    }

    pub fn opt(s: Option<String>) -> Json {
        s.map_or(Json::Null, Json::Str)
    }

    fn compact(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Int(n) => out.push_str(&n.to_string()),
            Json::Str(s) => quote(s, out),
            Json::Raw(r) => out.push_str(r),
            Json::Arr(items) => {
                out.push('[');
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    v.compact(out);
                }
                out.push(']');
            }
            Json::Obj(pairs) => {
                out.push('{');
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    quote(k, out);
                    out.push(':');
                    v.compact(out);
                }
                out.push('}');
            }
        }
    }

    /// What `json.Encoder` with a two-space indent writes: the compact form,
    /// re-indented, and a newline.
    pub fn encode(&self) -> String {
        let mut compact = String::new();
        self.compact(&mut compact);
        let mut out = indent(&compact);
        out.push('\n');
        out
    }
}

/// A string as Go writes one with HTML escaping on, which is the Encoder's
/// default: `<`, `>` and `&` go out as `<` and friends, and so do the two
/// line separators JavaScript cannot hold in a literal.
fn quote(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '<' | '>' | '&' | '\u{0}'..='\u{1f}' | '\u{2028}' | '\u{2029}' => {
                out.push_str(&format!("\\u{:04x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Go's `appendCompact` with HTML escaping: whitespace outside strings goes,
/// and inside them the bytes stay as sent — escapes included — bar the ones
/// `quote` escapes.
pub fn compact(raw: &[u8]) -> String {
    let mut out = Vec::with_capacity(raw.len());
    let (mut in_str, mut escaped) = (false, false);
    let mut i = 0;
    while i < raw.len() {
        let c = raw[i];
        if matches!(c, b'<' | b'>' | b'&') {
            out.extend_from_slice(format!("\\u00{c:02x}").as_bytes());
        } else if c == 0xE2 && raw.get(i + 1) == Some(&0x80) && raw.get(i + 2).is_some_and(|b| b & !1 == 0xA8) {
            out.extend_from_slice(format!("\\u202{:x}", raw[i + 2] & 0xF).as_bytes());
            i += 3;
            continue;
        } else if in_str {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_str = false;
            }
        } else if !matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
            in_str = c == b'"';
            out.push(c);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Go's `appendIndent` over compact input, with no prefix and two spaces.
fn indent(src: &str) -> String {
    let mut out = String::with_capacity(src.len() * 2);
    let (mut depth, mut need, mut in_str, mut escaped) = (0usize, false, false, false);
    let newline = |out: &mut String, depth: usize| {
        out.push('\n');
        out.extend(std::iter::repeat_n("  ", depth));
    };
    for c in src.chars() {
        if in_str {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        if need && c != ']' && c != '}' {
            need = false;
            depth += 1;
            newline(&mut out, depth);
        }
        match c {
            '"' => {
                in_str = true;
                out.push(c);
            }
            '{' | '[' => {
                need = true;
                out.push(c);
            }
            ',' => {
                out.push(c);
                newline(&mut out, depth);
            }
            ':' => out.push_str(": "),
            '}' | ']' => {
                if need {
                    need = false;
                } else {
                    depth -= 1;
                    newline(&mut out, depth);
                }
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out
}

/// The canonical text of a value for hashing: keys sorted at every level with
/// the last duplicate winning, numbers as written, strings re-escaped — Go's
/// `json.Marshal` of what `UseNumber` decoded.
pub fn canonical(v: &Value) -> String {
    let mut out = String::new();
    to_json(v).compact(&mut out);
    out
}

fn to_json(v: &Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Raw(b.to_string()),
        Value::Num(n) => Json::Raw(n.clone()),
        Value::Str(s) => Json::Str(s.clone()),
        Value::Arr(items) => Json::Arr(items.iter().map(to_json).collect()),
        Value::Obj(members) => {
            let mut last: Vec<(String, Json)> = Vec::new();
            for m in members {
                last.retain(|(k, _)| *k != m.key);
                last.push((m.key.clone(), to_json(&m.value)));
            }
            Json::map_of(last)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    #[test]
    fn output_is_indented_sorted_and_html_escaped() {
        let v = Json::map([("b", Json::Arr(vec![])), ("a", Json::str("<&>")), ("c", Json::Raw(compact(b"{ \"x\" : [1, 2.0] }")))]);
        assert_eq!(
            v.encode(),
            "{\n  \"a\": \"\\u003c\\u0026\\u003e\",\n  \"b\": [],\n  \"c\": {\n    \"x\": [\n      1,\n      2.0\n    ]\n  }\n}\n"
        );
    }

    #[test]
    fn canonical_sorts_and_keeps_number_spelling() {
        let a = parse(br#"{"b":1,"a":{"y":1.0,"x":"A"}}"#).unwrap();
        assert_eq!(canonical(&a), r#"{"a":{"x":"A","y":1.0},"b":1}"#);
    }
}
