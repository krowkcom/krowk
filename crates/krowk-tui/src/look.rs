//! The TUI's visual language: glyphs, colours, the spinner, durations, how
//! a tool call is named, and the light markdown an answer is shown in.
//!
//! The vocabulary follows Grok Build's (xAI, Apache-2.0: its
//! `xai-grok-pager-render` glyphs and "Terminal" theme, its minimal inline
//! mode's commit rules, its thinking and turn-status blocks): `❯` for what
//! the person said, `◆` for a tool, a tool shown once with its outcome,
//! thinking collapsed to "Thought for 4.2s", `Worked for 12s` after a turn,
//! ` │ ` between status items. Written here from those ideas; no code was
//! copied (see THIRD-PARTY-NOTICES).
//!
//! Colours are the terminal's own sixteen, so a person's theme decides what
//! they look like and a phone terminal shows them; the diff bands are two
//! 256-colour indexes that stay red and green where 256 colours degrade.

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

pub fn path() -> Style {
    Style::new().fg(Color::Cyan)
}

/// A URL printed into scrollback: written as an OSC 8 hyperlink to itself
/// (`term::write_styled`), and never wrapped by krowk, so the link is whole
/// wherever the terminal breaks it.
pub fn link() -> Style {
    Style::new().fg(Color::Cyan).add_modifier(Modifier::UNDERLINED)
}

/// Whether a span is a link: `link()`'s style on an http(s) URL.
pub fn is_link(span: &Span<'_>) -> bool {
    span.style == link() && (span.content.starts_with("https://") || span.content.starts_with("http://"))
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
        return Line::from(Span::styled(trimmed[hashes + 1..].to_string(), bold().fg(colour)));
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

/// `code` and `**bold**` within a line; anything unclosed stays as typed.
fn inline(text: &str) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut rest = text;
    loop {
        let tick = rest.find('`');
        let star = rest.find("**");
        let (at, marker) = match (tick, star) {
            (Some(t), Some(s)) if s < t => (s, "**"),
            (Some(t), _) => (t, "`"),
            (None, Some(s)) => (s, "**"),
            (None, None) => break,
        };
        let Some(close) = rest[at + marker.len()..].find(marker) else { break };
        let inner = &rest[at + marker.len()..at + marker.len() + close];
        if inner.is_empty() {
            break;
        }
        if at > 0 {
            out.push(Span::raw(rest[..at].to_string()));
        }
        out.push(Span::styled(inner.to_string(), if marker == "`" { code() } else { bold() }));
        rest = &rest[at + marker.len() * 2 + close..];
    }
    if !rest.is_empty() {
        out.push(Span::raw(rest.to_string()));
    }
    out
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
}
