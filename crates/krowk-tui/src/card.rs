//! A yes-or-no card asked before the TUI takes the terminal: the trust
//! question, in the TUI's look — a title, what it is about, and one key to
//! answer — rather than a line prompt waiting for Enter.

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use std::io::Write;

/// Draws `title` over `body` on stderr and waits for one key: `y` is yes,
/// any other no. What was typed or pasted before the card was drawn is
/// thrown away first, so a prompt typed ahead that has a `y` in it answers
/// nothing. Ctrl-C leaves krowk, as it would at any prompt. The answer is
/// left on screen, one line where the keys were.
pub fn confirm(title: &str, body: &[String], yes: &str, no: &str) -> bool {
    let raw = crossterm::terminal::enable_raw_mode().is_ok();
    // Before the card is drawn: only a key pressed after it answers it.
    while raw && event::poll(std::time::Duration::ZERO).unwrap_or(false) {
        let _ = event::read();
    }
    let mut err = std::io::stderr();
    let _ = write!(err, "\r\n  \x1b[1;35m{}\x1b[0m\r\n", clean(title));
    for line in body {
        let _ = write!(err, "  \x1b[2m{}\x1b[0m\r\n", clean(line));
    }
    let _ = write!(err, "\r\n  \x1b[1my\x1b[0m \x1b[2m{}\x1b[0m   \x1b[1mn\x1b[0m \x1b[2m{}\x1b[0m", clean(yes), clean(no));
    let _ = err.flush();
    let answer = raw && read_answer();
    let _ = crossterm::terminal::disable_raw_mode();
    let (glyph, word) = if answer { ("\x1b[32m✓\x1b[0m", yes) } else { ("\x1b[2m✗\x1b[0m", no) };
    let _ = write!(err, "\r\x1b[2K  {glyph} \x1b[2m{}\x1b[0m\r\n", clean(word));
    let _ = err.flush();
    answer
}

/// What a person pressed at a card of keys.
#[derive(Debug, PartialEq, Eq)]
pub enum Pressed {
    /// The key named `Enter` in the options.
    Enter,
    /// One of the options' single-character keys, lowercase.
    Key(char),
    /// Escape: not now.
    Escape,
}

/// Draws `title` over one line per option — `keys` names the key, `what`
/// says what it does — and waits for one of them: `"Enter"` or a single
/// character. Any other key is ignored; Escape is its own answer; Ctrl-C
/// leaves krowk, as it would at any prompt. What was typed before the card
/// was drawn is thrown away. The options stay on screen, as the trust card's
/// answer does.
pub fn pick(title: &str, options: &[(&str, &str)]) -> Pressed {
    let raw = crossterm::terminal::enable_raw_mode().is_ok();
    while raw && event::poll(std::time::Duration::ZERO).unwrap_or(false) {
        let _ = event::read();
    }
    let mut err = std::io::stderr();
    let _ = write!(err, "\r\n  \x1b[1;35m{}\x1b[0m\r\n\r\n", clean(title));
    for (keys, what) in options {
        let _ = write!(err, "  \x1b[1m[{}]\x1b[0m \x1b[2m{}\x1b[0m\r\n", clean(keys), clean(what));
    }
    let _ = err.flush();
    let pressed = if raw { read_key(options) } else { Pressed::Escape };
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = write!(err, "\r\n");
    let _ = err.flush();
    pressed
}

fn read_key(options: &[(&str, &str)]) -> Pressed {
    let wants = |c: char| options.iter().any(|(k, _)| k.chars().eq(std::iter::once(c)));
    let enter = options.iter().any(|(k, _)| *k == "Enter");
    loop {
        match event::read() {
            Ok(Event::Key(k)) if k.kind != KeyEventKind::Release => match k.code {
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    let _ = crossterm::terminal::disable_raw_mode();
                    let _ = write!(std::io::stderr(), "\r\n");
                    std::process::exit(130);
                }
                KeyCode::Enter if enter => break Pressed::Enter,
                KeyCode::Esc => break Pressed::Escape,
                KeyCode::Char(c) if (k.modifiers - KeyModifiers::SHIFT).is_empty() && wants(c.to_ascii_lowercase()) => break Pressed::Key(c.to_ascii_lowercase()),
                _ => {}
            },
            Ok(_) => {}
            Err(_) => break Pressed::Escape,
        }
    }
}

fn read_answer() -> bool {
    loop {
        match event::read() {
            Ok(Event::Key(k)) if k.kind != KeyEventKind::Release => match k.code {
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    let _ = crossterm::terminal::disable_raw_mode();
                    let _ = write!(std::io::stderr(), "\r\n");
                    std::process::exit(130);
                }
                KeyCode::Char('y' | 'Y') if (k.modifiers - KeyModifiers::SHIFT).is_empty() => break true,
                _ => break false,
            },
            Ok(Event::Paste(_)) => break false,
            Ok(_) => {}
            Err(_) => break false,
        }
    }
}

/// Text only: a path or a vendor's name never carries an escape sequence,
/// or a bidirectional control that would show it as another name.
pub fn clean(s: &str) -> String {
    s.chars().filter(|c| !c.is_control() && !is_bidi(*c)).collect()
}

/// The invisible format characters that reorder or hide text.
pub fn is_bidi(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}')
}
