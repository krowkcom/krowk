//! The TUI's visual language: glyphs, colours, the spinner, durations, how
//! a tool call is named, and the light markdown an answer is shown in.
//!
//! The vocabulary follows Grok Build's (xAI, Apache-2.0: its
//! `xai-grok-pager-render` glyphs and "Terminal" theme, its minimal inline
//! mode's commit rules, its turn-status block): `❯` for what the person
//! said, `◆` for a tool, a tool shown once with its outcome,
//! `Worked for 12s` after a turn,
//! ` │ ` between status items. Written here from those ideas; no code was
//! copied (see THIRD-PARTY-NOTICES).
//!
//! Colours are the terminal's own sixteen, so a person's theme decides what
//! they look like and a phone terminal shows them; the diff bands are two
//! 256-colour indexes that stay red and green where 256 colours degrade.
//! Most of what is shown is ink on the terminal's paper — its foreground,
//! full or washed (dim) — never the white or black slots, which are
//! surfaces in one mode or the other; a hue is for what needs the eye.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::time::Duration;

pub const PROMPT: &str = "❯ ";
/// Before the prompt's first row, inside its box.
pub const ARROW: &str = "→ ";
pub const TOOL: &str = "◆ ";
/// Before each line under a tool call, the way a file tree draws a
/// directory's entries: the last one closes the branch.
pub const BRANCH: &str = "├─ ";
pub const LAST_BRANCH: &str = "└─ ";
pub const WARN: &str = "⚠ ";
/// Before what a command did, as `krowk connect` prints it.
pub const DONE: &str = "✓ ";
pub const STOPPED: &str = "◌ ";
pub const STEER: &str = "↳ ";
/// Before a switch of model, instance or engine.
pub const SWITCH: &str = "⇄ ";
pub const SEP: &str = " │ ";

/// Braille spinner, one frame per `SPIN_FRAME` while a turn runs.
pub const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
pub const SPIN_FRAME: Duration = Duration::from_millis(125);

pub fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

pub fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

pub fn accent() -> Style {
    Style::new().fg(Color::Magenta)
}

pub fn success() -> Style {
    Style::new().fg(Color::Green)
}

pub fn error() -> Style {
    Style::new().fg(Color::Red)
}

pub fn warning() -> Style {
    Style::new().fg(Color::Yellow)
}

/// A tool call's `◆` while it runs; once its result is back the glyph goes
/// to the ink. The yellow slot, which spalvos — the palette this is tuned
/// on — fills with its orange.
pub fn running() -> Style {
    Style::new().fg(Color::Yellow)
}

/// What a tool call acts on — a path, a command, a pattern: the ink washed,
/// not a colour, which is kept for what needs the eye.
pub fn path() -> Style {
    dim()
}

/// A link printed into scrollback, written as an OSC 8 hyperlink
/// (`term::write_styled`). A URL shown as itself is never wrapped by krowk,
/// so it is whole wherever the terminal breaks it.
pub fn link() -> Style {
    Style::new().fg(Color::Cyan).add_modifier(Modifier::UNDERLINED)
}

/// After a link in an answer, in the link's colour but not underlined: it
/// opens the link too.
pub const LINK_ARROW: &str = "\u{a0}↗";

/// Not underlined: said as a removed modifier, which draws as nothing, so
/// the arrow's style is its own and never `code()`'s.
fn link_arrow() -> Style {
    Style::new().fg(Color::Cyan).remove_modifier(Modifier::UNDERLINED)
}

/// Where a span links to, and the text it shows: `link()`'s style on an
/// http(s) URL links to itself; a `linked` span to the URL it carries.
pub fn link_target<'a>(span: &'a Span<'_>) -> Option<(&'a str, String)> {
    if span.style != link() && span.style != link_arrow() {
        return None;
    }
    let content = span.content.as_ref();
    if let Some(at) = content.find(is_tag) {
        let url: String = content[at..].chars().filter(|c| is_tag(*c)).filter_map(|c| char::from_u32(u32::from(c) - TAG)).collect();
        return is_url(&url).then(|| (&content[..at], url));
    }
    (span.style == link() && is_url(content)).then(|| (content, content.to_string()))
}

fn is_url(s: &str) -> bool {
    s.starts_with("https://") || s.starts_with("http://")
}

/// Unicode tag characters: ASCII shifted to U+E0000, zero columns wide and
/// drawn as nothing. A span's link target rides behind its text as these,
/// since a span has nowhere else to keep it, and `link_target` reads it back.
const TAG: u32 = 0xE0000;

fn is_tag(c: char) -> bool {
    (TAG + 0x21..=TAG + 0x7E).contains(&u32::from(c))
}

/// `s` without tag characters: text from outside (an answer, a sign-in URL)
/// may not carry a link target of its own.
pub fn untagged(s: &str) -> String {
    s.replace(|c: char| (TAG..TAG + 0x80).contains(&u32::from(c)), "")
}

/// `text` in `style`, linking to `url` (an http(s) URL). Anything but
/// printable ASCII in the URL is percent-encoded, so no escape or control
/// character can reach the terminal inside it.
pub fn linked(text: String, style: Style, url: &str) -> Span<'static> {
    let mut content = text;
    content.extend(encoded(url).chars().filter_map(|c| char::from_u32(TAG + u32::from(c))));
    Span::styled(content, style)
}

/// `url` as printable ASCII: every other byte percent-encoded.
fn encoded(url: &str) -> String {
    url.bytes().map(|b| if (0x21..=0x7E).contains(&b) { char::from(b).to_string() } else { format!("%{b:02X}") }).collect()
}

/// Whether a link shows its own URL (`link_target`'s text and target), as a
/// bare one does: then it is left whole for the terminal to wrap, so it
/// copies as one URL.
pub fn shows_its_url(text: &str, url: &str) -> bool {
    text == url || (is_url(text) && encoded(text) == url)
}

/// A link in an answer: its text in `link()`, then `↗`, both opening `url`.
fn link_spans(text: &str, url: &str) -> [Span<'static>; 2] {
    [linked(text.to_string(), link(), url), linked(LINK_ARROW.to_string(), link_arrow(), url)]
}

pub fn code() -> Style {
    Style::new().fg(Color::Cyan)
}

/// The prompt box's frame.
pub fn border() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

pub fn prompt() -> Style {
    Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD)
}

pub fn insert_band() -> Style {
    Style::new().bg(Color::Indexed(22))
}

pub fn delete_band() -> Style {
    Style::new().bg(Color::Indexed(52))
}

/// `4.2s`, `12s`, `1m5s`, `1h2m`.
pub fn duration(d: Duration) -> String {
    let s = d.as_secs_f64();
    match d.as_secs() {
        0..=9 => format!("{s:.1}s"),
        10..=59 => format!("{}s", d.as_secs()),
        60..=3599 => format!("{}m{}s", d.as_secs() / 60, d.as_secs() % 60),
        n => format!("{}h{}m", n / 3600, (n % 3600) / 60),
    }
}

/// The name of krowk's own tool that a vendor's tool is: Claude Code's
/// `Bash`, `Read`, `Edit`… and Codex's `shell`, so a call through
/// `claude:` or `codex:` is shown the way a native one is. Any other name
/// is its own.
pub fn tool_kind(name: &str) -> &str {
    match name {
        "Bash" | "shell" => "bash",
        "Read" => "read",
        "Write" => "write",
        "Grep" => "grep",
        "Glob" => "glob",
        "Edit" => "search_replace",
        "TodoWrite" => "todo_write",
        "Skill" => "skill",
        other => other,
    }
}

/// How a tool call is named: a verb, and what it acts on.
pub fn tool_title(name: &str, input: &serde_json::Value) -> (String, String) {
    let s = |k: &str| input.get(k).and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let path = || Some(s("path")).filter(|p| !p.is_empty()).unwrap_or_else(|| s("file_path"));
    let first_line = |t: String| t.lines().next().unwrap_or_default().to_string();
    match tool_kind(name) {
        "read" => ("Read".into(), path()),
        "write" => ("Write".into(), path()),
        "bash" => ("Run".into(), first_line(s("command"))),
        "grep" => ("Search".into(), s("pattern")),
        "glob" => ("Find".into(), s("pattern")),
        "todo_write" => ("Plan".into(), String::new()),
        "subagent" => ("Agent".into(), s("description")),
        "skill" => labelled("Skill".into(), Some(s("name")).filter(|n| !n.is_empty()).unwrap_or_else(|| s("skill"))),
        "str_replace" => ("Edit".into(), s("path")),
        "search_replace" => ("Edit".into(), s("file_path")),
        "apply_patch" => ("Edit".into(), patch_paths(&s("input")).join(", ")),
        other => {
            let arg = ["command", "path", "file_path", "pattern", "url"].iter().map(|k| s(k)).find(|v| !v.is_empty()).unwrap_or_default();
            labelled(words(other), first_line(arg))
        }
    }
}

/// A name that isn't a verb, set off from what it acts on:
/// `Skill: basecamp`, `Web Fetch: https://…`.
fn labelled(name: String, arg: String) -> (String, String) {
    if arg.is_empty() { (name, arg) } else { (format!("{name}:"), arg) }
}

/// `ListAgents` as `List Agents`, `HTTPGet` as `HTTP Get`: a word starts at
/// a capital after a lowercase letter or digit, or at the last capital of a
/// run followed by a lowercase letter.
fn words(name: &str) -> String {
    let c: Vec<char> = name.chars().collect();
    let mut out = String::with_capacity(name.len() + 4);
    for (i, &ch) in c.iter().enumerate() {
        let starts = i > 0 && ch.is_uppercase() && (c[i - 1].is_lowercase() || c[i - 1].is_ascii_digit() || (c[i - 1].is_uppercase() && c.get(i + 1).is_some_and(|n| n.is_lowercase())));
        if starts {
            out.push(' ');
        }
        out.push(ch);
    }
    out
}

fn patch_paths(patch: &str) -> Vec<String> {
    patch
        .lines()
        .filter_map(|l| ["*** Update File: ", "*** Add File: ", "*** Delete File: "].iter().find_map(|p| l.strip_prefix(p)))
        .map(|p| p.trim().to_string())
        .collect()
}

/// The lines an edit call removes and adds, as the model asked for them:
/// `old_str`/`new_str`, `old_string`/`new_string`, or a patch's `-` and `+`.
pub fn edit_lines(name: &str, input: &serde_json::Value) -> Option<(Vec<String>, Vec<String>)> {
    let s = |k: &str| input.get(k).and_then(|v| v.as_str()).map(String::from);
    let split = |t: String| t.lines().map(String::from).collect::<Vec<_>>();
    match tool_kind(name) {
        "str_replace" => Some((split(s("old_str")?), split(s("new_str")?))),
        "search_replace" => Some((split(s("old_string")?), split(s("new_string")?))),
        "apply_patch" => {
            let p = s("input")?;
            let body = p.lines().filter(|l| !l.starts_with("***") && !l.starts_with("@@"));
            let (mut del, mut add) = (Vec::new(), Vec::new());
            for l in body {
                if let Some(r) = l.strip_prefix('-') {
                    del.push(r.to_string());
                } else if let Some(a) = l.strip_prefix('+') {
                    add.push(a.to_string());
                }
            }
            Some((del, add))
        }
        _ => None,
    }
}

/// One line of an answer in light markdown: headings bold and coloured,
/// list markers as `•`, quotes behind a rule, `code` and fenced blocks in
/// the code colour, `**bold**` bold. Line by line, so it streams: `fence`
/// carries whether a fenced block is open across lines.
pub fn markdown_line(text: &str, fence: &mut bool) -> Line<'static> {
    let text: &str = &untagged(text);
    let trimmed = text.trim_start();
    if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
        *fence = !*fence;
        return Line::from(Span::styled(text.to_string(), dim()));
    }
    if *fence {
        return Line::from(Span::styled(text.to_string(), code()));
    }
    let indent = &text[..text.len() - trimmed.len()];
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
        let colour = match hashes {
            1 => Color::Cyan,
            2 => Color::Blue,
            _ => Color::Magenta,
        };
        return Line::from(emphasised(inline(&trimmed[hashes + 1..]), bold().fg(colour)));
    }
    let mut spans = vec![Span::raw(indent.to_string())];
    let body = if let Some(rest) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")).or_else(|| trimmed.strip_prefix("+ ")) {
        spans.push(Span::styled("• ", accent()));
        rest
    } else if let Some(rest) = trimmed.strip_prefix("> ") {
        spans.push(Span::styled("│ ", dim()));
        rest
    } else {
        trimmed
    };
    spans.extend(inline(body));
    Line::from(spans)
}

/// `code`, `**bold**` and links within a line: `[text](url)`, `<url>` and a
/// bare URL, each opening its http(s) URL (`link_spans`). Anything unclosed,
/// and a link to anything but http(s), stays as typed.
fn inline(text: &str) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut plain = 0;
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        let token = if rest.starts_with('`') || rest.starts_with("**") {
            let marker = if rest.starts_with('`') { "`" } else { "**" };
            rest[marker.len()..]
                .find(marker)
                .filter(|&close| close > 0)
                .map(|close| {
                    let inner = &rest[marker.len()..marker.len() + close];
                    let spans = if marker == "`" { vec![Span::styled(inner.to_string(), code())] } else { emphasised(inline(inner), bold()) };
                    (marker.len() * 2 + close, spans)
                })
        } else {
            link_at(rest).map(|(len, label, url)| (len, link_spans(&label, url).to_vec()))
        };
        match token {
            Some((len, spans)) => {
                if plain < i {
                    out.push(Span::raw(text[plain..i].to_string()));
                }
                out.extend(spans);
                i += len;
                plain = i;
            }
            None => i += rest.chars().next().map_or(1, char::len_utf8),
        }
    }
    if plain < text.len() {
        out.push(Span::raw(text[plain..].to_string()));
    }
    out
}

/// The longest URL looked for: a scan for a URL's end stops here, so a
/// line of unclosed brackets costs a bounded look from each.
const URL_MAX: usize = 2048;

/// `spans` in `style`, each keeping its own on top; a link keeps its look
/// exactly, which is what makes it one (`link_target`).
fn emphasised(spans: Vec<Span<'static>>, style: Style) -> Vec<Span<'static>> {
    spans.into_iter().map(|s| if link_target(&s).is_some() { s } else { Span::styled(s.content, style.patch(s.style)) }).collect()
}

/// Whether `url` names a host: `http://` alone opens nothing.
fn has_host(url: &str) -> bool {
    url.split_once("://").is_some_and(|(_, host)| !host.is_empty() && !host.starts_with('/'))
}

/// A link at the start of `s`: how many bytes it takes, the text it shows
/// and the URL it opens.
fn link_at(s: &str) -> Option<(usize, String, &str)> {
    if let Some(rest) = s.strip_prefix('[') {
        // The text ends at the first bracket: `[ ] a [b](…)` links `b`. Any
        // other `[` stops the look, so each run between brackets is read once.
        let close = rest.find(['[', ']']).filter(|&c| rest[c..].starts_with("]("))?;
        let label = &rest[..close];
        let after = &rest[close + 2..];
        // A URL may hold balanced parentheses, as Wikipedia's do; it ends
        // by the next `[`, where the next link would start.
        let mut depth = 0;
        let end = after.char_indices().take_while(|&(j, c)| j < URL_MAX && !c.is_whitespace() && c != '[').find_map(|(j, c)| match c {
            '(' => {
                depth += 1;
                None
            }
            ')' if depth == 0 => Some(j),
            ')' => {
                depth -= 1;
                None
            }
            _ => None,
        })?;
        let url = &after[..end];
        let label = label.replace("**", "").replace('`', "");
        return (!label.trim().is_empty() && is_url(url) && has_host(url)).then_some((1 + close + 2 + end + 1, label, url));
    }
    if let Some(rest) = s.strip_prefix('<') {
        if !is_url(rest) {
            return None;
        }
        let end = rest.char_indices().take_while(|&(j, c)| j < URL_MAX && !c.is_whitespace() && c != '<').find(|&(_, c)| c == '>')?.0;
        let url = &rest[..end];
        return has_host(url).then(|| (end + 2, url.to_string(), url));
    }
    // Its host looked at first: a line of `http:///` costs a look at each.
    if !is_url(s) || !s[s.find("://")? + 3..].starts_with(|c: char| c != '/' && !c.is_whitespace()) {
        return None;
    }
    let end = s.char_indices().take_while(|&(j, c)| j < URL_MAX && !c.is_whitespace()).last().map_or(0, |(j, c)| j + c.len_utf8());
    let mut url = &s[..end];
    // Punctuation after a URL ends the sentence, not the URL; so does a
    // closing bracket the URL did not open. The brackets are counted once
    // and kept count of as the end is taken off, so a run of them costs a
    // look each.
    let count = |c: char| url.matches(c).count();
    let (mut parens, mut squares) = (count(')') as isize - count('(') as isize, count(']') as isize - count('[') as isize);
    loop {
        let trimmed = url.trim_end_matches(['.', ',', ';', ':', '!', '?', '\'', '"', '*', '`', '>']);
        let trimmed = if trimmed.ends_with(')') && parens > 0 {
            parens -= 1;
            &trimmed[..trimmed.len() - 1]
        } else if trimmed.ends_with(']') && squares > 0 {
            squares -= 1;
            &trimmed[..trimmed.len() - 1]
        } else {
            trimmed
        };
        if trimmed == url {
            break;
        }
        url = trimmed;
    }
    has_host(url).then(|| (url.len(), url.to_string(), url))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn durations_read_like_a_person_says_them() {
        assert_eq!(duration(Duration::from_millis(4230)), "4.2s");
        assert_eq!(duration(Duration::from_secs(12)), "12s");
        assert_eq!(duration(Duration::from_secs(65)), "1m5s");
        assert_eq!(duration(Duration::from_secs(3720)), "1h2m");
    }

    #[test]
    fn tools_are_named_by_what_they_do() {
        assert_eq!(tool_title("read", &json!({"path": "README.md"})), ("Read".into(), "README.md".into()));
        assert_eq!(tool_title("bash", &json!({"command": "cargo test\necho done"})), ("Run".into(), "cargo test".into()));
        assert_eq!(tool_title("apply_patch", &json!({"input": "*** Begin Patch\n*** Update File: a.rs\n*** Add File: b.rs\n"})), ("Edit".into(), "a.rs, b.rs".into()));
        let (del, add) = edit_lines("str_replace", &json!({"path": "x", "old_str": "a\nb", "new_str": "c"})).unwrap();
        assert_eq!((del, add), (vec!["a".to_string(), "b".into()], vec!["c".to_string()]));
        let (del, add) = edit_lines("apply_patch", &json!({"input": "*** Begin Patch\n*** Update File: x\n@@ ctx\n keep\n-old\n+new\n*** End Patch"})).unwrap();
        assert_eq!((del, add), (vec!["old".to_string()], vec!["new".to_string()]));
    }

    #[test]
    fn a_vendors_tools_are_named_as_krowks_own() {
        assert_eq!(tool_title("Read", &json!({"file_path": "/r/README.md"})), ("Read".into(), "/r/README.md".into()));
        assert_eq!(tool_title("Bash", &json!({"command": "ls\npwd"})), ("Run".into(), "ls".into()));
        assert_eq!(tool_title("shell", &json!({"command": "ls", "cwd": "/r"})), ("Run".into(), "ls".into()));
        assert_eq!(tool_title("Grep", &json!({"pattern": "fn test"})), ("Search".into(), "fn test".into()));
        assert_eq!(tool_title("TodoWrite", &json!({"todos": []})), ("Plan".into(), String::new()));
        assert_eq!(tool_title("skill", &json!({"name": "basecamp"})), ("Skill:".into(), "basecamp".into()));
        assert_eq!(tool_title("Skill", &json!({"skill": "basecamp"})), ("Skill:".into(), "basecamp".into()));
        assert_eq!(tool_title("ListAgents", &json!({})), ("List Agents".into(), String::new()));
        assert_eq!(tool_title("WebFetch", &json!({"url": "https://x"})), ("Web Fetch:".into(), "https://x".into()));
        assert_eq!(tool_title("HTTPGet", &json!({})), ("HTTP Get".into(), String::new()));
        assert_eq!(tool_title("web_search", &json!({})), ("web_search".into(), String::new()));
        let (del, add) = edit_lines("Edit", &json!({"file_path": "x", "old_string": "a", "new_string": "b\nc"})).unwrap();
        assert_eq!((del, add), (vec!["a".to_string()], vec!["b".to_string(), "c".into()]));
        assert_eq!(tool_title("mcp__gh__search", &json!({})).0, "mcp__gh__search", "one krowk has no name for is its own");
    }

    #[test]
    fn markdown_is_shown_line_by_line() {
        let mut f = false;
        assert_eq!(text(&markdown_line("## Usage", &mut f)), "Usage");
        assert_eq!(text(&markdown_line("  - one `two` **three**", &mut f)), "  • one two three");
        let l = markdown_line("use `krowk push` now", &mut f);
        assert_eq!(l.spans[2].style, code(), "{:?}", l.spans);
        assert_eq!(text(&markdown_line("```rust", &mut f)), "```rust");
        assert!(f, "the fence is open");
        assert_eq!(text(&markdown_line("- not a list in code", &mut f)), "- not a list in code");
        markdown_line("```", &mut f);
        assert!(!f);
        assert_eq!(text(&markdown_line("a ` lone tick and **unclosed", &mut f)), "a ` lone tick and **unclosed");
        assert_eq!(text(&markdown_line("line 00001: the quick brown fox", &mut f)), "line 00001: the quick brown fox");
    }

    fn shown(l: &Line) -> String {
        l.spans.iter().map(|s| link_target(s).map_or(s.content.as_ref(), |(t, _)| t)).collect()
    }

    fn targets(l: &Line) -> Vec<String> {
        l.spans.iter().filter_map(|s| link_target(s).map(|(_, u)| u)).collect()
    }

    #[test]
    fn a_link_shows_its_text_and_an_arrow_and_opens_its_url() {
        let mut f = false;
        let l = markdown_line("see [the **docs**](https://krowk.com/docs) now", &mut f);
        assert_eq!(shown(&l), "see the docs\u{a0}↗ now");
        assert_eq!(targets(&l), ["https://krowk.com/docs"; 2], "the text and the arrow both open it");
        assert_eq!(l.spans[2].style, link(), "{:?}", l.spans);
        assert_eq!(l.width(), "see the docs ↗ now".chars().count(), "the URL takes no columns");
        let l = markdown_line("[Rust](https://en.wikipedia.org/wiki/Rust_(programming_language)).", &mut f);
        assert_eq!((shown(&l).as_str(), targets(&l)[0].as_str()), ("Rust\u{a0}↗.", "https://en.wikipedia.org/wiki/Rust_(programming_language)"));
        let l = markdown_line("at https://krowk.com/a, or (<http://x.io>)", &mut f);
        assert_eq!(shown(&l), "at https://krowk.com/a\u{a0}↗, or (http://x.io\u{a0}↗)");
        assert_eq!(targets(&l), ["https://krowk.com/a", "https://krowk.com/a", "http://x.io", "http://x.io"]);
        let l = markdown_line("[b](file:///etc/passwd) [c](src/main.rs) `https://x.io`", &mut f);
        assert!(targets(&l).is_empty(), "only http(s), and not in code: {:?}", l.spans);
        assert_eq!(shown(&l), "[b](file:///etc/passwd) [c](src/main.rs) https://x.io");
        let smuggled = format!("`x{}`", linked(String::new(), link(), "https://evil.io").content);
        assert!(targets(&markdown_line(&smuggled, &mut f)).is_empty(), "no link smuggled in from the answer");
    }

    #[test]
    fn a_bracket_before_a_link_is_left_alone() {
        let mut f = false;
        let l = markdown_line("- [ ] fix [docs](https://x.io) [1] see [here](https://y.io)", &mut f);
        assert_eq!(shown(&l), "• [ ] fix docs\u{a0}↗ [1] see here\u{a0}↗");
        let l = markdown_line("[see https://x.io] and <https://y.io>>", &mut f);
        assert_eq!(shown(&l), "[see https://x.io\u{a0}↗] and https://y.io\u{a0}↗>");
        assert_eq!(targets(&l), ["https://x.io", "https://x.io", "https://y.io", "https://y.io"]);
        let l = markdown_line("http://[::1]:8080/a.", &mut f);
        assert_eq!(targets(&l)[0], "http://[::1]:8080/a");
    }

    #[test]
    fn a_line_of_unclosed_brackets_takes_linear_time() {
        let mut f = false;
        let trailing = |c: &str| format!("http://x{}", c.repeat(80_000));
        for line in ["[1,".repeat(40_000), "a <".repeat(40_000), "[a](x".repeat(20_000), "<http://x".repeat(20_000), trailing(")"), trailing("]"), trailing(".)"), "http:///".repeat(40_000), "<https:///".repeat(40_000)] {
            let t = std::time::Instant::now();
            markdown_line(&line, &mut f);
            assert!(t.elapsed() < Duration::from_millis(500), "{:?} for {:?}…", t.elapsed(), &line[..12]);
        }
    }

    #[test]
    fn a_link_in_bold_or_a_heading_is_a_link_too() {
        let mut f = false;
        let l = markdown_line("- **[Title](https://x.io)** — desc", &mut f);
        assert_eq!((shown(&l).as_str(), targets(&l)), ("• Title\u{a0}↗ — desc", vec!["https://x.io".to_string(); 2]));
        let l = markdown_line("**See [docs](https://x.io) and `this`**", &mut f);
        assert_eq!(shown(&l), "See docs\u{a0}↗ and this");
        assert!(l.spans.iter().filter(|s| link_target(s).is_none() && !s.content.is_empty()).all(|s| s.style.add_modifier.contains(Modifier::BOLD)), "{:?}", l.spans);
        assert_eq!(l.spans.iter().find(|s| s.content == "this").unwrap().style.fg, Some(Color::Cyan), "code keeps its colour in bold");
        let l = markdown_line("## See [docs](https://x.io)", &mut f);
        assert_eq!((shown(&l).as_str(), targets(&l).len()), ("See docs\u{a0}↗", 2));
        assert_eq!(l.spans[0].style, bold().fg(Color::Blue));
        let l = markdown_line("<http://> [a](http://) [b](https:///x)", &mut f);
        assert!(targets(&l).is_empty(), "no host, no link: {:?}", l.spans);
    }

    #[test]
    fn only_an_http_target_is_read_back() {
        let code = Span::styled(format!("x{}", linked(String::new(), link(), "https://evil.io").content), code());
        assert!(link_target(&code).is_none(), "a code span is never a link, whatever it carries");
        assert!(link_target(&linked("https://ok.io".into(), link(), "file:///etc/passwd")).is_none());
        let signin = untagged(&format!("https://ok.io{}", linked(String::new(), link(), "https://evil.io").content));
        assert_eq!(link_target(&Span::styled(signin, link())).unwrap().1, "https://ok.io");
    }

    #[test]
    fn a_links_url_carries_no_control_character() {
        let s = linked("x".into(), link(), "https://x.io/\x1b]0;pwned\x07/é");
        assert_eq!(link_target(&s).unwrap().1, "https://x.io/%1B]0;pwned%07/%C3%A9");
    }
}
