//! The prompt: a multi-line editor with readline's keys and a history.
//!
//! Enter sends; Shift-Enter, Alt-Enter, Ctrl-J, or a backslash before Enter
//! starts a new line instead (Shift-Enter on Windows and where the terminal
//! took the keyboard protocol push; elsewhere, and on every phone keyboard,
//! it is Enter). A paste arrives whole, newlines
//! and all, through bracketed paste. Up and Down move between lines, and
//! past the first or last line walk the history.
//!
//! The history is a file of JSON strings, one per line, so a multi-line
//! prompt comes back as it was sent. It is appended to as prompts are sent
//! and read once at start, the newest `HISTORY_KEPT` only.

use std::io::Write;
use std::path::PathBuf;
use unicode_width::UnicodeWidthChar;

pub const HISTORY_KEPT: usize = 500;

#[derive(Debug, Default)]
pub struct Editor {
    text: String,
    /// A byte offset into `text`, always on a char boundary.
    cursor: usize,
    history: Vec<String>,
    /// Which history entry is shown, counted back from the newest; none
    /// while editing a fresh prompt.
    browsing: Option<usize>,
    /// The prompt being written when browsing began, restored after the
    /// newest entry.
    draft: String,
    file: Option<PathBuf>,
}

impl Editor {
    pub fn new(file: Option<PathBuf>) -> Editor {
        let history = file.as_deref().map(load_history).unwrap_or_default();
        Editor { history, file, ..Editor::default() }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.browsing = None;
    }

    /// Takes the prompt for sending and records it in the history.
    pub fn take(&mut self) -> String {
        let text = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.browsing = None;
        if !text.trim().is_empty() && self.history.last() != Some(&text) {
            self.history.push(text.clone());
            if let Some(path) = &self.file {
                append_history(path, &text);
            }
        }
        text
    }

    /// Puts `text` back at the front of the prompt, above whatever is being
    /// written, with the caret at the end of the whole.
    pub fn restore(&mut self, text: &str) {
        self.browsing = None;
        self.text = if self.text.is_empty() { text.to_string() } else { format!("{text}\n{}", self.text) };
        self.cursor = self.text.len();
    }

    pub fn insert(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// A paste, or anything else that arrives as a string. Carriage returns
    /// become newlines, and tabs four spaces, so what is shown is what is
    /// sent.
    pub fn insert_str(&mut self, s: &str) {
        let clean: String = s.replace("\r\n", "\n").replace('\r', "\n").replace('\t', "    ").chars().filter(|c| *c == '\n' || !c.is_control()).collect();
        self.text.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
    }

    /// Enter: true when the prompt is to be sent. A backslash right before
    /// the cursor is a line continuation, and is replaced by the newline.
    pub fn enter(&mut self) -> bool {
        if self.text[..self.cursor].ends_with('\\') {
            self.cursor -= 1;
            self.text.remove(self.cursor);
            self.insert('\n');
            return false;
        }
        true
    }

    pub fn backspace(&mut self) {
        if let Some(c) = self.text[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
            self.text.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            self.text.remove(self.cursor);
        }
    }

    pub fn left(&mut self) {
        if let Some(c) = self.text[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
        }
    }

    pub fn right(&mut self) {
        if let Some(c) = self.text[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
        }
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..].find('\n').map_or(self.text.len(), |i| self.cursor + i)
    }

    pub fn home(&mut self) {
        self.cursor = self.line_start();
    }

    pub fn end(&mut self) {
        self.cursor = self.line_end();
    }

    pub fn word_left(&mut self) {
        let before = &self.text[..self.cursor];
        let trimmed = before.trim_end_matches(|c: char| !c.is_alphanumeric());
        self.cursor = trimmed.rfind(|c: char| !c.is_alphanumeric()).map_or(0, |i| i + trimmed[i..].chars().next().map_or(1, char::len_utf8));
    }

    pub fn word_right(&mut self) {
        let after = &self.text[self.cursor..];
        let skip = after.find(|c: char| c.is_alphanumeric()).unwrap_or(after.len());
        let rest = &after[skip..];
        let word = rest.find(|c: char| !c.is_alphanumeric()).unwrap_or(rest.len());
        self.cursor += skip + word;
    }

    /// Ctrl-U: everything from the start of the line to the cursor.
    pub fn kill_to_start(&mut self) {
        let start = self.line_start();
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    /// Ctrl-K: everything from the cursor to the end of the line.
    pub fn kill_to_end(&mut self) {
        let end = self.line_end();
        self.text.replace_range(self.cursor..end, "");
    }

    /// Ctrl-W: the word before the cursor.
    pub fn kill_word(&mut self) {
        let end = self.cursor;
        self.word_left();
        self.text.replace_range(self.cursor..end, "");
    }

    /// Up: the line above, or from the first line the previous prompt sent.
    pub fn up(&mut self) {
        let start = self.line_start();
        if start == 0 {
            self.history_back();
            return;
        }
        let col = self.text[start..self.cursor].chars().count();
        let prev_start = self.text[..start - 1].rfind('\n').map_or(0, |i| i + 1);
        self.cursor = char_offset(&self.text, prev_start, start - 1, col);
    }

    /// Down: the line below, or from the last line the next prompt sent.
    pub fn down(&mut self) {
        let end = self.line_end();
        if end == self.text.len() {
            self.history_forward();
            return;
        }
        let col = self.text[self.line_start()..self.cursor].chars().count();
        let next_start = end + 1;
        let next_end = self.text[next_start..].find('\n').map_or(self.text.len(), |i| next_start + i);
        self.cursor = char_offset(&self.text, next_start, next_end, col);
    }

    fn history_back(&mut self) {
        let next = self.browsing.map_or(0, |i| i + 1);
        if next >= self.history.len() {
            return;
        }
        if self.browsing.is_none() {
            self.draft = std::mem::take(&mut self.text);
        }
        self.browsing = Some(next);
        self.text = self.history[self.history.len() - 1 - next].clone();
        self.cursor = self.text.len();
    }

    fn history_forward(&mut self) {
        match self.browsing {
            None => {}
            Some(0) => {
                self.browsing = None;
                self.text = std::mem::take(&mut self.draft);
                self.cursor = self.text.len();
            }
            Some(i) => {
                self.browsing = Some(i - 1);
                self.text = self.history[self.history.len() - i].clone();
                self.cursor = self.text.len();
            }
        }
    }

    /// The prompt as rows `width` columns wide, and the caret's row and
    /// column among them. Each logical line wraps on its own; a wide char
    /// that does not fit moves to the next row whole.
    pub fn layout(&self, width: u16) -> (Vec<String>, (u16, u16)) {
        let width = usize::from(width.max(2));
        let mut rows = vec![String::new()];
        let mut col = 0usize;
        let mut caret = (0u16, 0u16);
        let mut at = 0usize;
        for c in self.text.chars().chain(std::iter::once('\u{0}')) {
            if at == self.cursor {
                if col >= width {
                    rows.push(String::new());
                    col = 0;
                }
                caret = ((rows.len() - 1) as u16, col as u16);
            }
            if c == '\u{0}' {
                break;
            }
            at += c.len_utf8();
            if c == '\n' {
                rows.push(String::new());
                col = 0;
                continue;
            }
            let w = c.width().unwrap_or(0);
            if col + w > width {
                rows.push(String::new());
                col = 0;
            }
            rows.last_mut().expect("rows is never empty").push(c);
            col += w;
        }
        (rows, caret)
    }
}

/// The byte offset `col` chars into the line `start..end`, or its end.
fn char_offset(text: &str, start: usize, end: usize, col: usize) -> usize {
    text[start..end].char_indices().nth(col).map_or(end, |(i, _)| start + i)
}

fn load_history(path: &std::path::Path) -> Vec<String> {
    let Ok(raw) = std::fs::read_to_string(path) else { return Vec::new() };
    let mut all: Vec<String> = raw.lines().filter_map(|l| serde_json::from_str::<String>(l).ok()).collect();
    let skip = all.len().saturating_sub(HISTORY_KEPT);
    all.drain(..skip);
    all
}

/// One prompt onto the history file. A history that cannot be written costs
/// the history, never the prompt.
fn append_history(path: &std::path::Path, text: &str) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    if let Ok(mut f) = opts.open(path) {
        let _ = writeln!(f, "{}", serde_json::Value::String(text.into()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(s: &str) -> Editor {
        let mut e = Editor::new(None);
        e.insert_str(s);
        e
    }

    #[test]
    fn a_backslash_before_enter_continues_the_prompt_on_a_new_line() {
        let mut e = typed("first \\");
        assert!(!e.enter());
        e.insert_str("second");
        assert_eq!(e.text(), "first \nsecond");
        assert!(e.enter());
    }

    #[test]
    fn a_paste_keeps_its_lines_and_loses_its_control_characters() {
        let e = typed("a\r\nb\tc\x1b[31md");
        assert_eq!(e.text(), "a\nb    c[31md");
    }

    #[test]
    fn up_and_down_move_between_lines_then_walk_the_history() {
        let mut e = Editor::new(None);
        e.insert_str("one");
        e.take();
        e.insert_str("two\nlines");
        e.take();
        e.insert_str("draft");
        e.up();
        assert_eq!(e.text(), "two\nlines", "the newest entry first");
        e.up();
        assert_eq!(&e.text()[..e.cursor()], "two", "then the line above, same column");
        e.up();
        assert_eq!(e.text(), "one");
        e.down();
        assert_eq!(e.text(), "two\nlines");
        e.down();
        e.down();
        assert_eq!(e.text(), "draft", "past the newest, the draft is back");
    }

    #[test]
    fn unread_steering_goes_back_above_the_draft() {
        let mut e = typed("draft");
        e.restore("also check the tests");
        assert_eq!(e.text(), "also check the tests\ndraft");
        let mut e = Editor::new(None);
        e.restore("only this");
        assert_eq!((e.text(), e.cursor()), ("only this", 9));
    }

    #[test]
    fn readline_kills_and_word_moves() {
        let mut e = typed("hello big world");
        e.kill_word();
        assert_eq!(e.text(), "hello big ");
        e.word_left();
        assert_eq!(e.cursor(), 6);
        e.kill_to_end();
        assert_eq!(e.text(), "hello ");
        e.kill_to_start();
        assert_eq!(e.text(), "");
        let mut e = typed("héllo wörld");
        e.home();
        e.word_right();
        assert_eq!(&e.text()[..e.cursor()], "héllo");
        e.backspace();
        assert_eq!(e.text(), "héll wörld");
    }

    #[test]
    fn layout_wraps_each_line_and_places_the_caret() {
        let mut e = typed("abcdef\nxy");
        let (rows, caret) = e.layout(4);
        assert_eq!(rows, ["abcd", "ef", "xy"]);
        assert_eq!(caret, (2, 2));
        e.home();
        e.up();
        let (_, caret) = e.layout(4);
        assert_eq!(caret, (0, 0));
        let e = typed("abcd");
        assert_eq!(e.layout(4).1, (1, 0), "a caret past a full row starts the next");
        let e = typed("日本語");
        assert_eq!(e.layout(4).0, ["日本", "語"], "wide chars are two columns");
    }

    #[test]
    fn history_is_kept_as_json_lines_and_read_back() {
        let dir = std::env::temp_dir().join(format!("krowk-tui-history-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("history.jsonl");
        let mut e = Editor::new(Some(path.clone()));
        e.insert_str("multi\nline");
        e.take();
        e.insert_str("multi\nline");
        e.take();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw, "\"multi\\nline\"\n", "a repeat is not recorded twice");
        let mut again = Editor::new(Some(path));
        again.up();
        assert_eq!(again.text(), "multi\nline");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
