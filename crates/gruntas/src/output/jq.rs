//! --jq: a jq expression run over the JSON a command rendered, in process.
//!
//! What it does not buy is a second contract: every command renders what it
//! would have, and the filter reads that. The process environment is out of
//! reach — `env` and `$ENV` answer `{}` — because GRUNTAS_TOKEN lives there and
//! an expression is a string that travels: into a skill file, a CI job, a
//! command one agent hands another.

use gruntas_api::{fail, Error};
use jaq_core::load::{Arena, File, Loader};
use jaq_core::{data, Compiler, Ctx, Vars};
use jaq_json::Val;
use regex_lite::Regex;
use std::sync::LazyLock;
use std::time::Duration;

/// One run's ceiling. An expression that recurses without a base case would
/// otherwise wedge an unattended container with no diagnostic.
const DEADLINE: Duration = Duration::from_secs(30);

/// A --jq expression that compiled.
#[derive(Debug)]
pub struct Filter {
    expr: String,
}

/// Parses and compiles before the command does anything, so a typo is refused
/// before an upload lands. `given` is whether the flag appeared at all: an
/// empty `--jq "$FIELD"` is a shell that expanded a variable into nothing, and
/// answering it with the whole envelope would hand over what it asked a field of.
pub fn compile(expr: &str, given: bool) -> Result<Option<Filter>, Error> {
    if !given {
        return Ok(None);
    }
    if expr.trim().is_empty() {
        return Err(fail("bad_jq", "a blank --jq is not an expression — pass one, e.g. --jq '.data.artifacts[].slug'"));
    }
    with_filter(expr, |_| ()).map_err(|why| fail("bad_jq", format!("--jq: {why}")))?;
    Ok(Some(Filter { expr: expr.to_string() }))
}

/// Whether a failure is one --jq caused — the one kind a filter may not be run
/// over, since it would bury the news.
pub fn is_filter_failure(err: &Error) -> bool {
    matches!(err.code().as_str(), "bad_jq" | "jq_failed" | "jq_unsupported")
}

type Compiled = jaq_core::compile::Filter<jaq_core::Native<data::JustLut<Val>>>;

/// What jq has and jaq does not, defined the way jq defines it, plus `env`
/// answering `{}`.
const COMPAT_DEFS: &str = r#"
def env: {};
def tostream: path(def r: (.[]?|r), .; r) as $p | getpath($p) | reduce path(.[]?) as $q ([$p, .]; [$p+$q]);
def __krowk_csv: if type != "array" then error("\(type) (\(tojson)) cannot be csv-formatted, only an array can be")
  else map(if type == "string" then "\"" + gsub("\""; "\"\"") + "\""
    elif type == "null" then "" elif type == "array" or type == "object" then error("\(type) (\(tojson)) is not valid in a csv row")
    else tojson end) | join(",") end;
def __krowk_tsv: if type != "array" then error("\(type) (\(tojson)) cannot be tsv-formatted, only an array can be")
  else map(if type == "string" then gsub("\\\\"; "\\\\") | gsub("\t"; "\\t") | gsub("\n"; "\\n") | gsub("\r"; "\\r")
    elif type == "null" then "" elif type == "array" or type == "object" then error("\(type) (\(tojson)) is not valid in a tsv row")
    else tojson end) | join("\t") end;
"#;

/// `@csv` and `@tsv` spelled as the definitions above: jaq parses a format
/// it does not know as an error, so the expression is rewritten before it is
/// loaded. Code inside a string's `\(…)` is code and is rewritten too, and the
/// format-string form `@csv "row: \(.x)"` pipes each interpolation through the
/// format, as jq does.
fn with_formats(expr: &str) -> String {
    let chars: Vec<char> = expr.chars().collect();
    let mut out = String::new();
    rewrite_code(&chars, &mut 0, &mut out, false);
    out
}

fn format_def(word: &str) -> Option<&'static str> {
    match word {
        "csv" => Some("__krowk_csv"),
        "tsv" => Some("__krowk_tsv"),
        _ => None,
    }
}

/// Copies code from `chars[*i..]` into `out`, rewriting formats. Inside an
/// interpolation (`nested`) it stops at the `)` that closes it.
fn rewrite_code(chars: &[char], i: &mut usize, out: &mut String, nested: bool) {
    let mut depth = 0usize;
    while *i < chars.len() {
        let c = chars[*i];
        match c {
            '"' => {
                *i += 1;
                rewrite_string(chars, i, out, None);
                continue;
            }
            '(' => depth += 1,
            ')' if nested && depth == 0 => return,
            ')' => depth = depth.saturating_sub(1),
            '@' => {
                let start = *i + 1;
                let mut end = start;
                while end < chars.len() && chars[end].is_ascii_alphanumeric() {
                    end += 1;
                }
                let word: String = chars[start..end].iter().collect();
                if let Some(def) = format_def(&word) {
                    // `@csv "…"` is the format-string form: the format applies
                    // to what each interpolation yields, not to the string.
                    let mut next = end;
                    while next < chars.len() && chars[next].is_whitespace() {
                        next += 1;
                    }
                    *i = end;
                    if next < chars.len() && chars[next] == '"' {
                        *i = next + 1;
                        rewrite_string(chars, i, out, Some(def));
                    } else {
                        out.push_str(def);
                    }
                    continue;
                }
            }
            _ => {}
        }
        out.push(c);
        *i += 1;
    }
}

/// Copies a string literal, from just after its opening quote through its
/// closing one. Interpolated code is rewritten, and under a format-string
/// piped through the format.
fn rewrite_string(chars: &[char], i: &mut usize, out: &mut String, format: Option<&str>) {
    out.push('"');
    while *i < chars.len() {
        let c = chars[*i];
        *i += 1;
        match c {
            '\\' if chars.get(*i) == Some(&'(') => {
                *i += 1;
                out.push_str("\\(");
                if format.is_some() {
                    out.push('(');
                }
                rewrite_code(chars, i, out, true);
                if let Some(def) = format {
                    out.push_str(&format!(")|{def}"));
                }
                if *i < chars.len() {
                    out.push(')');
                    *i += 1;
                }
            }
            '\\' => {
                out.push(c);
                if let Some(&escaped) = chars.get(*i) {
                    out.push(escaped);
                    *i += 1;
                }
            }
            '"' => {
                out.push('"');
                return;
            }
            c => out.push(c),
        }
    }
}

/// Loads and compiles `expr` with jq's standard library, `env` answering `{}`,
/// and hands the result to `f`. The error is a sentence about where it gave up.
fn with_filter<T>(expr: &str, f: impl FnOnce(&Compiled) -> T) -> Result<T, String> {
    let arena = Arena::default();
    let expr = &with_formats(expr);
    let env_def = jaq_core::load::parse(COMPAT_DEFS, |p| p.defs()).unwrap_or_default();
    let defs = jaq_core::defs().chain(jaq_std::defs()).chain(jaq_json::defs()).chain(env_def);
    let funs = jaq_core::funs().chain(jaq_std::funs()).chain(jaq_json::funs()).filter(|(name, _, _)| *name != "env");
    let modules = Loader::new(defs).load(&arena, File { code: expr, path: () }).map_err(|errs| {
        errs.into_iter()
            .map(|(_, e)| match e {
                jaq_core::load::Error::Io(v) => v.into_iter().map(|(_, s)| s).collect::<Vec<_>>().join("; "),
                jaq_core::load::Error::Lex(v) => {
                    v.iter().map(|(want, at)| format!("expected {} at `{}`", want.as_str(), clip(at))).collect::<Vec<_>>().join("; ")
                }
                jaq_core::load::Error::Parse(v) => {
                    v.iter().map(|(want, at)| format!("expected {} at `{}`", want.as_str(), clip(at))).collect::<Vec<_>>().join("; ")
                }
            })
            .collect::<Vec<_>>()
            .join("; ")
    })?;
    let filter = Compiler::default().with_funs(funs).with_global_vars(["$ENV"]).compile(modules).map_err(|errs| {
        errs.into_iter()
            .flat_map(|(_, undefined)| undefined)
            .map(|(name, why)| format!("undefined {} `{name}`", why.as_str()))
            .collect::<Vec<_>>()
            .join("; ")
    })?;
    Ok(f(&filter))
}

fn clip(s: &str) -> String {
    if s.is_empty() {
        return "the end".into();
    }
    s.chars().take(20).collect()
}

impl Filter {
    /// Runs over one rendered result and returns what it wrote — one value per
    /// line, a string as itself — and how many values were not null. A caller
    /// deciding whether the filter had anything to say needs that count: an
    /// expression written for a result answers `null` over a failure.
    ///
    /// Nothing is returned when a later value fails, so half a listing never
    /// reaches stdout ahead of an exit code.
    pub fn write(&self, rendered: &str, tty: bool) -> Result<(String, usize), Error> {
        let (expr, rendered) = (self.expr.clone(), rendered.to_string());
        let (tx, rx) = std::sync::mpsc::channel();
        // ponytail: a filter past the deadline is abandoned, not stopped; the
        // process is about to exit with the failure anyway.
        std::thread::spawn(move || {
            let _ = tx.send(run(&expr, &rendered, tty));
        });
        rx.recv_timeout(DEADLINE).unwrap_or_else(|_| {
            Err(fail(
                "jq_failed",
                "--jq did not finish within 30s — an expression that never ends, such as one recursing without a base case",
            ))
        })
    }
}

fn run(expr: &str, rendered: &str, tty: bool) -> Result<(String, usize), Error> {
    // Keys sorted before the filter sees them, as they are for gojq, which
    // holds a decoded object as a map: `.[]`, `to_entries` and `paths` then
    // walk them in the order the Go build did.
    let sorted: serde_json::Value = serde_json::from_str(rendered)
        .map(|v| sort_keys(&v))
        .map_err(|e| fail("jq_failed", format!("--jq had no JSON result to filter: {e}")))?;
    let input = jaq_json::read::parse_single(sorted.to_string().as_bytes())
        .map_err(|e| fail("jq_failed", format!("--jq had no JSON result to filter: {e}")))?;
    with_filter(expr, |filter| {
        let ctx = Ctx::<data::JustLut<Val>>::new(&filter.lut, Vars::new([Val::obj(Default::default())]));
        let (mut out, mut said) = (String::new(), 0);
        for v in filter.id.run((ctx, input)) {
            let v = match v {
                Ok(v) => v,
                Err(exn) => {
                    return Err(match exn.get_err() {
                        Ok(e) => fail("jq_failed", format!("--jq: {}", without_credentials(&e.to_string()))),
                        // halt and halt_error would choose the exit code, and
                        // krowk's exit codes are what happened to the artifact.
                        Err(_) => fail(
                            "jq_failed",
                            "--jq: halt and halt_error are not honoured — in jq they choose the process's exit code, \
                             and krowk's exit codes are what happened to the artifact",
                        ),
                    })
                }
            };
            if !matches!(v, Val::Null) {
                said += 1;
            }
            let line = match &v {
                Val::TStr(s) => {
                    let s = String::from_utf8_lossy(s);
                    if tty { terminal_safe_string(&s) } else { s.into_owned() }
                }
                other => {
                    let mut encoded = String::new();
                    encode(other, &mut encoded);
                    if tty { terminal_safe_json(&encoded) } else { encoded }
                }
            };
            out.push_str(&line);
            out.push('\n');
        }
        Ok((out, said))
    })
    .map_err(|why| fail("bad_jq", format!("--jq: {why}")))?
}

fn sort_keys(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            serde_json::Value::Object(keys.into_iter().map(|k| (k.clone(), sort_keys(&map[k]))).collect())
        }
        serde_json::Value::Array(items) => serde_json::Value::Array(items.iter().map(sort_keys).collect()),
        other => other.clone(),
    }
}

/// A result as JSON: keys sorted, as gojq writes them, and a number JSON cannot
/// spell written as the nearest one it can — NaN as null, an infinity as the
/// largest finite double — so what comes out always parses.
fn encode(v: &Val, out: &mut String) {
    match v {
        Val::Num(jaq_json::Num::Float(f)) if f.is_nan() => out.push_str("null"),
        Val::Num(jaq_json::Num::Float(f)) if f.is_infinite() => {
            out.push_str(if *f > 0.0 { "1.7976931348623157e+308" } else { "-1.7976931348623157e+308" })
        }
        Val::Num(jaq_json::Num::Float(f)) => out.push_str(&go_float(*f)),
        // A literal jaq keeps as its digits, gojq holds as a float64.
        Val::Num(jaq_json::Num::Dec(d)) => match d.parse::<f64>() {
            Ok(f) if f.is_finite() => out.push_str(&go_float(f)),
            Ok(f) => encode(&Val::Num(jaq_json::Num::Float(f)), out),
            Err(_) => out.push_str(d),
        },
        Val::Arr(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                encode(item, out);
            }
            out.push(']');
        }
        Val::Obj(map) => {
            let mut entries: Vec<(String, &Val)> = map.iter().map(|(k, v)| (key_text(k), v)).collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            out.push('{');
            for (i, (k, v)) in entries.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::Value::String(k.clone()).to_string());
                out.push(':');
                encode(v, out);
            }
            out.push('}');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// A float the way Go's encoder writes one, which is what a shell reading
/// `$(krowk … --jq 'length/2')` gets: no `.0` on an integral value, positional
/// digits below 1e21, and `1e+21`-style exponents outside that range.
fn go_float(f: f64) -> String {
    let abs = f.abs();
    if abs != 0.0 && !(1e-6..1e21).contains(&abs) {
        let s = format!("{f:e}");
        let (mantissa, exp) = s.split_once('e').unwrap_or((&s, "0"));
        return match exp.strip_prefix('-') {
            Some(neg) => format!("{mantissa}e-{neg}"),
            None => format!("{mantissa}e+{exp:0>2}"),
        };
    }
    format!("{f}")
}

fn key_text(k: &Val) -> String {
    match k {
        Val::TStr(s) | Val::BStr(s) => String::from_utf8_lossy(s).into_owned(),
        other => other.to_string(),
    }
}

/// The two secrets krowk mints, redacted to their prefix, and the message
/// capped: `error(.)` hands back the whole document otherwise.
fn without_credentials(message: &str) -> String {
    static SECRET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(krowk_(?:sk|claim)_)[A-Za-z0-9_-]+").unwrap());
    let message = SECRET.replace_all(message, "${1}[redacted]");
    const LONGEST: usize = 400;
    if message.len() <= LONGEST {
        return message.into_owned();
    }
    let mut cut = LONGEST;
    while !message.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{} …", &message[..cut])
}

fn unprintable(c: char) -> bool {
    c.is_control() || crate::termclean::reordering(c)
}

/// A string safe to print on a terminal without changing what it says: left
/// alone when nothing in it could repaint the row, escaped when something can.
fn terminal_safe_string(s: &str) -> String {
    if !s.chars().any(unprintable) {
        return s.to_string();
    }
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x80 && unprintable(c) => out.push_str(&format!("\\x{:02x}", c as u32)),
            c if unprintable(c) => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// The same job on an encoded compound result: JSON's structure is printable
/// ASCII, so anything replaced is inside a string, spelled as JSON spells it.
fn terminal_safe_json(encoded: &str) -> String {
    encoded.chars().map(|c| if unprintable(c) { format!("\\u{:04x}", c as u32) } else { c.to_string() }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(expr: &str) -> Filter {
        compile(expr, true).unwrap().unwrap()
    }

    #[test]
    fn strings_print_raw_and_compound_values_as_json() {
        let doc = r#"{"data":{"artifacts":[{"slug":"art_1","byte_size":5}]}}"#;
        assert_eq!(filter(".data.artifacts[0].slug").write(doc, false).unwrap(), ("art_1\n".into(), 1));
        let (out, said) = filter(".data.artifacts[0] | {byte_size}").write(doc, false).unwrap();
        assert_eq!((serde_json::from_str::<serde_json::Value>(&out).unwrap()["byte_size"].as_i64(), said), (Some(5), 1));
        assert_eq!(filter(".data.artifacts[] | select(.byte_size > 100)").write(doc, false).unwrap(), (String::new(), 0));
        assert_eq!(filter(".missing").write(doc, false).unwrap(), ("null\n".into(), 0));
    }

    #[test]
    fn what_jq_has_and_jaq_does_not_answers_as_jq_does() {
        assert_eq!(filter(r#"[1,"a,b",null,"q\"t"] | @csv"#).write("null", false).unwrap().0, "1,\"a,b\",,\"q\"\"t\"\n");
        assert_eq!(filter(r#"["a\tb",2] | @tsv"#).write("null", false).unwrap().0, "a\\tb\t2\n");
        assert_eq!(filter(r#""@csv stays text""#).write("null", false).unwrap().0, "@csv stays text\n");
        assert_eq!(filter(r#""\([1,"a"]|@csv)""#).write("null", false).unwrap().0, "1,\"a\"\n");
        assert_eq!(filter(r#"@csv "row: \(.)""#).write(r#"[1,"a"]"#, false).unwrap().0, "row: 1,\"a\"\n");
        assert_eq!(filter("[10/2, 5*1.0, 1e17, 0.5, 1e21, 1e-7]").write("null", false).unwrap().0, "[5,5,100000000000000000,0.5,1e+21,1e-7]\n");
        assert_eq!(filter("10/2").write("null", false).unwrap().0, "5\n");
        assert_eq!(filter("[.[] | tostream]").write("[[1]]", false).unwrap().0, "[[[0],1],[[0]]]\n");
        assert_eq!(filter("[nan, infinite, -infinite]").write("null", false).unwrap().0, "[null,1.7976931348623157e+308,-1.7976931348623157e+308]\n");
        assert_eq!(filter("to_entries[0].key").write(r#"{"b":1,"a":2}"#, false).unwrap().0, "a\n");
        assert_eq!(filter("{z: 1, a: 2}").write("null", false).unwrap().0, "{\"a\":2,\"z\":1}\n");
    }

    #[test]
    fn the_environment_is_out_of_reach() {
        assert_eq!(filter("env").write("{}", false).unwrap().0, "{}\n");
        assert_eq!(filter("$ENV.HOME").write("{}", false).unwrap().0, "null\n");
    }

    #[test]
    fn failures_are_classified_and_never_carry_a_credential() {
        assert_eq!(compile("", true).unwrap_err().code(), "bad_jq");
        assert_eq!(compile(".[[[", true).unwrap_err().code(), "bad_jq");
        assert!(compile(".", false).unwrap().is_none());
        let e = filter("error").write(r#""krowk_sk_abc_def-1 leaked""#, false).unwrap_err();
        assert_eq!(e.code(), "jq_failed");
        assert!(!e.fix().contains("abc_def") && e.fix().contains("krowk_sk_[redacted]"), "{}", e.fix());
        assert!(filter("halt_error").write("1", false).unwrap_err().fix().contains("not honoured"));
    }

    #[test]
    fn a_terminal_never_sees_what_could_repaint_it() {
        assert_eq!(filter(".").write(r#""\u001b[31mred""#, true).unwrap().0, "\\x1b[31mred\n");
        assert_eq!(filter(".").write(r#""plain  name""#, true).unwrap().0, "plain  name\n");
        assert_eq!(filter("[.]").write(r#""a\u202eb""#, true).unwrap().0, "[\"a\\u202eb\"]\n");
    }
}
