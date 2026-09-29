//! Code in colour: a fenced block in an answer, a file a call wrote.
//!
//! The grammars are bat's (two-face's syntax set, Sublime Text's grammars
//! and more: TypeScript, TOML, Dockerfile), read by syntect. The theme is
//! the terminal's own colours, like the rest of the TUI (`look`): a scope
//! names a slot of the sixteen, never an RGB, so a person's theme decides
//! what code looks like. Both are loaded the first time code is shown.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use std::str::FromStr;
use std::sync::LazyLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::{self as hl, FontStyle, ScopeSelectors, StyleModifier, Theme, ThemeItem, ThemeSettings};
use syntect::parsing::{SyntaxReference, SyntaxSet};

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_newlines);
static THEME: LazyLock<Theme> = LazyLock::new(theme);

/// A theme's colour is a slot here, not a colour: `a` says which kind.
const SLOT: u8 = 0;
/// The terminal's foreground.
const INK: u8 = 1;
/// The foreground, washed.
const WASHED: u8 = 2;

fn slot(n: u8) -> hl::Color {
    hl::Color { r: n, g: 0, b: 0, a: SLOT }
}

fn theme() -> Theme {
    let (red, green, yellow, blue, magenta, cyan) = (slot(1), slot(2), slot(3), slot(4), slot(5), slot(6));
    let washed = hl::Color { r: 0, g: 0, b: 0, a: WASHED };
    let rules: [(&str, hl::Color, FontStyle); 16] = [
        ("comment, punctuation.definition.comment", washed, FontStyle::ITALIC),
        ("string, constant.character, punctuation.definition.string", green, FontStyle::empty()),
        ("constant.numeric, constant.language, constant.other", yellow, FontStyle::empty()),
        ("keyword, storage.type, storage.modifier", magenta, FontStyle::empty()),
        ("keyword.operator", hl::Color { r: 0, g: 0, b: 0, a: INK }, FontStyle::empty()),
        ("entity.name.function, support.function, meta.function-call variable.function", blue, FontStyle::empty()),
        ("entity.name.type, entity.name.class, entity.name.struct, entity.name.enum, entity.name.trait, support.type, support.class, entity.name.namespace", cyan, FontStyle::empty()),
        ("entity.name.tag", blue, FontStyle::empty()),
        ("entity.other.attribute-name", yellow, FontStyle::empty()),
        ("variable.other.constant, constant.other.caps", yellow, FontStyle::empty()),
        ("entity.name.section, markup.heading", blue, FontStyle::BOLD),
        ("markup.inserted", green, FontStyle::empty()),
        ("markup.deleted", red, FontStyle::empty()),
        ("markup.bold", hl::Color { r: 0, g: 0, b: 0, a: INK }, FontStyle::BOLD),
        ("markup.italic", hl::Color { r: 0, g: 0, b: 0, a: INK }, FontStyle::ITALIC),
        ("invalid", red, FontStyle::empty()),
    ];
    Theme {
        settings: ThemeSettings { foreground: Some(hl::Color { r: 0, g: 0, b: 0, a: INK }), ..ThemeSettings::default() },
        scopes: rules
            .into_iter()
            .filter_map(|(scope, fg, font)| {
                Some(ThemeItem { scope: ScopeSelectors::from_str(scope).ok()?, style: StyleModifier { foreground: Some(fg), background: None, font_style: Some(font) } })
            })
            .collect(),
        ..Theme::default()
    }
}

/// A theme's style as the terminal's: its slot, or the ink.
fn style(s: hl::Style) -> Style {
    let c = s.foreground;
    let mut out = match c.a {
        SLOT => Style::new().fg(match c.r {
            1 => Color::Red,
            2 => Color::Green,
            3 => Color::Yellow,
            4 => Color::Blue,
            5 => Color::Magenta,
            _ => Color::Cyan,
        }),
        WASHED => Style::new().add_modifier(Modifier::DIM),
        _ => Style::new(),
    };
    if s.font_style.contains(FontStyle::BOLD) {
        out = out.add_modifier(Modifier::BOLD);
    }
    if s.font_style.contains(FontStyle::ITALIC) {
        out = out.add_modifier(Modifier::ITALIC);
    }
    out
}

/// The grammar a fence's info string names: a language (`rust`, `ts`) or
/// a path (`src/main.rs`, `12:20:src/main.rs`), by its name or extension.
fn syntax(info: &str) -> Option<&'static SyntaxReference> {
    let word = info.split_whitespace().next()?;
    let path = word.rsplit(':').next().unwrap_or(word);
    let file = path.rsplit('/').next().unwrap_or(path);
    let ext = file.rsplit_once('.').map_or(file, |(_, e)| e);
    SYNTAXES.find_syntax_by_token(word).or_else(|| SYNTAXES.find_syntax_by_extension(file)).or_else(|| SYNTAXES.find_syntax_by_extension(ext))
}

/// Code highlighted line by line, as it streams: each line's colours
/// follow from the lines before it (a string or comment left open).
pub struct Code(Option<HighlightLines<'static>>);

impl std::fmt::Debug for Code {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() { "Code(highlighted)" } else { "Code(plain)" })
    }
}

impl Code {
    /// Code in the language `info` names (a fence's info string, or a
    /// path); in the ink when it names none that is known.
    pub fn new(info: &str) -> Self {
        Code(syntax(info).map(|s| HighlightLines::new(s, &THEME)))
    }

    /// One line of it, without its newline, as spans in the theme.
    pub fn line(&mut self, text: &str) -> Vec<Span<'static>> {
        let Some(h) = &mut self.0 else {
            return vec![Span::raw(text.to_string())];
        };
        let with_newline = format!("{text}\n");
        match h.highlight_line(&with_newline, &SYNTAXES) {
            Ok(parts) => parts
                .into_iter()
                .map(|(s, t)| Span::styled(t.trim_end_matches('\n').to_string(), style(s)))
                .filter(|s| !s.content.is_empty())
                .collect(),
            Err(_) => vec![Span::raw(text.to_string())],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fence_names_its_grammar_by_language_or_path() {
        for info in ["rust", "rs", "src/main.rs", "12:20:src/main.rs", "ts", "toml", "Dockerfile", "sh", "python title=x"] {
            assert!(syntax(info).is_some(), "{info}");
        }
        assert!(syntax("no-such-language").is_none());
        assert!(syntax("").is_none());
    }

    #[test]
    fn code_is_coloured_from_the_terminals_slots() {
        let mut c = Code::new("rust");
        let spans = c.line("fn main() { let s = \"hi\"; } // done");
        let of = |t: &str| spans.iter().find(|s| s.content.contains(t)).map(|s| s.style).unwrap();
        assert_eq!(of("fn").fg, Some(Color::Magenta));
        assert_eq!(of("main").fg, Some(Color::Blue));
        assert_eq!(of("hi").fg, Some(Color::Green));
        assert!(of("done").add_modifier.contains(Modifier::DIM));
        assert_eq!(spans.iter().map(|s| s.content.as_ref()).collect::<String>(), "fn main() { let s = \"hi\"; } // done");
    }

    #[test]
    fn a_comment_left_open_colours_the_lines_after_it() {
        let mut c = Code::new("c");
        c.line("/* open");
        assert!(c.line("still in it").iter().all(|s| s.style.add_modifier.contains(Modifier::DIM)));
    }

    #[test]
    fn an_unknown_language_is_the_ink() {
        assert_eq!(Code::new("zzz").line("x = 1"), vec![Span::raw("x = 1")]);
    }
}

