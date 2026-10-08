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
//!
//! A pasted image is `[Image #N]` in the text (`paste`), and one unit to
//! the caret: it never lands inside one, Backspace and Delete take it
//! whole, and the arrows and word moves step over it.

use std::io::Write;
use std::ops::Range;
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
    /// Where pastes still being read go: a mark per paste, left at the
    /// caret as it was asked for, moved with the text around it.
    marks: Vec<(u64, usize)>,
    next_mark: u64,
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
        self.marks.clear();
    }

    /// Replaces `range` with `with`, every mark moving with the text: one
    /// in what was removed to where it was, one after it on by the
    /// difference, and one where text is only inserted staying before it,
    /// so what is typed after Ctrl-V lands after the paste.
    fn splice(&mut self, range: Range<usize>, with: &str) {
        let (start, end) = (range.start, range.end);
        self.text.replace_range(range, with);
        for (_, at) in &mut self.marks {
            if *at < start || (*at == start && start == end) {
                continue;
            }
            *at = if *at >= end { *at - (end - start) + with.len() } else { start };
        }
    }

    /// A mark at the caret, for a paste still being read (`place_image`,
    /// `place_str`).
    pub fn mark(&mut self) -> u64 {
        self.next_mark += 1;
        self.marks.push((self.next_mark, self.cursor));
        self.next_mark
    }

    /// Where mark `id` is, letting it go: at the caret when it is gone
    /// with the prompt it was in (sent, cleared).
    fn take_mark(&mut self, id: Option<u64>) -> usize {
        let at = id.and_then(|id| self.marks.iter().position(|(m, _)| *m == id)).map(|i| self.marks.remove(i).1);
        // Never past the text, inside a character or inside a token.
        at.filter(|a| self.text.is_char_boundary(*a) && self.inside(*a).is_none()).unwrap_or(self.cursor)
    }

    /// Inserts `s` at `at`: the caret, if it was there or past it, and
    /// any mark left there since moving on with it, so pastes land in the
    /// order they were asked for.
    fn place(&mut self, at: usize, s: &str) {
        self.text.insert_str(at, s);
        for (_, m) in &mut self.marks {
            if *m >= at {
                *m += s.len();
            }
        }
        if self.cursor >= at {
            self.cursor += s.len();
        }
    }

    /// `[Image #n]` where mark `id` was left, a space either side of it
    /// where the text has none.
    pub fn place_image(&mut self, id: Option<u64>, n: u32) {
        self.place_images(id, &[n]);
    }

    /// Several, as one paste of several files brings them: in order, a
    /// space between each.
    pub fn place_images(&mut self, id: Option<u64>, numbers: &[u32]) {
        let at = self.take_mark(id);
        if numbers.is_empty() {
            return;
        }
        let before = self.text[..at].chars().next_back().is_some_and(|c| !c.is_whitespace());
        let after = !self.text[at..].starts_with(char::is_whitespace);
        let tokens: Vec<String> = numbers.iter().map(|n| image_token(*n)).collect();
        self.place(at, &format!("{}{}{}", if before { " " } else { "" }, tokens.join(" "), if after { " " } else { "" }));
    }

    /// A paste's text where mark `id` was left, as `insert_str` cleans it.
    pub fn place_str(&mut self, id: Option<u64>, s: &str) {
        let at = self.take_mark(id);
        self.place(at, &clean(s));
    }

    /// Lets go of mark `id`: its paste brought nothing.
    pub fn unmark(&mut self, id: u64) {
        self.marks.retain(|(m, _)| *m != id);
    }

    /// Takes the prompt for sending and records it in the history.
    pub fn take(&mut self) -> String {
        let text = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.browsing = None;
        self.marks.clear();
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
        // Above the whole prompt, a paste's mark at its start included.
        let above = if self.text.is_empty() { text.to_string() } else { format!("{text}\n") };
        self.text.insert_str(0, &above);
        for (_, at) in &mut self.marks {
            *at += above.len();
        }
        self.cursor = self.text.len();
    }

    pub fn insert(&mut self, c: char) {
        self.splice(self.cursor..self.cursor, c.encode_utf8(&mut [0; 4]));
        self.cursor += c.len_utf8();
    }

    /// `[Image #n]` at the caret, a space either side of it where the text
    /// has none, and the caret after it.
    pub fn insert_image(&mut self, n: u32) {
        let at = self.mark();
        self.place_image(Some(at), n);
    }

    /// The image token the caret is strictly inside, if any.
    fn inside(&self, at: usize) -> Option<Range<usize>> {
        image_tokens(&self.text).map(|(r, _)| r).find(|r| r.start < at && at < r.end)
    }

    /// Moves the caret out of a token it landed in: to its start when
    /// moving back, its end when moving on.
    fn snap(&mut self, back: bool) {
        if let Some(r) = self.inside(self.cursor) {
            self.cursor = if back { r.start } else { r.end };
        }
    }

    /// A paste, or anything else that arrives as a string. Carriage returns
    /// become newlines, and tabs four spaces, so what is shown is what is
    /// sent.
    pub fn insert_str(&mut self, s: &str) {
        let clean = clean(s);
        self.splice(self.cursor..self.cursor, &clean);
        self.cursor += clean.len();
    }

    /// Enter: true when the prompt is to be sent. A backslash right before
    /// the cursor is a line continuation, and is replaced by the newline.
    pub fn enter(&mut self) -> bool {
        if self.text[..self.cursor].ends_with('\\') {
            self.splice(self.cursor - 1..self.cursor, "\n");
            return false;
        }
        true
    }

    pub fn backspace(&mut self) {
        let at = self.cursor;
        let token = image_tokens(&self.text).map(|(r, _)| r).find(|r| r.end == at);
        if let Some(r) = token {
            self.cursor = r.start;
            self.splice(r, "");
            return;
        }
        if let Some(c) = self.text[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
            self.splice(self.cursor..self.cursor + c.len_utf8(), "");
        }
    }

    pub fn delete(&mut self) {
        let at = self.cursor;
        let token = image_tokens(&self.text).map(|(r, _)| r).find(|r| r.start == at);
        if let Some(r) = token {
            self.splice(r, "");
            return;
        }
        if let Some(c) = self.text[self.cursor..].chars().next() {
            self.splice(self.cursor..self.cursor + c.len_utf8(), "");
        }
    }

    pub fn left(&mut self) {
        if let Some(c) = self.text[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
        }
        self.snap(true);
    }

    pub fn right(&mut self) {
        if let Some(c) = self.text[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
        }
        self.snap(false);
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
        self.snap(true);
    }

    pub fn word_right(&mut self) {
        let after = &self.text[self.cursor..];
        let skip = after.find(|c: char| c.is_alphanumeric()).unwrap_or(after.len());
        let rest = &after[skip..];
        let word = rest.find(|c: char| !c.is_alphanumeric()).unwrap_or(rest.len());
        self.cursor += skip + word;
        self.snap(false);
    }

    /// Ctrl-U: everything from the start of the line to the cursor.
    pub fn kill_to_start(&mut self) {
        let start = self.line_start();
        self.splice(start..self.cursor, "");
        self.cursor = start;
    }

    /// Ctrl-K: everything from the cursor to the end of the line.
    pub fn kill_to_end(&mut self) {
        let end = self.line_end();
        self.splice(self.cursor..end, "");
    }

    /// Ctrl-W: the word before the cursor.
    pub fn kill_word(&mut self) {
        let end = self.cursor;
        self.word_left();
        self.splice(self.cursor..end, "");
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
        self.snap(false);
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
        self.snap(false);
    }

    /// Up in the prompt as it is shown at `width`: the row above, the
    /// screen's wrapping one as much as a new line, at the same column or
    /// the row's end; from the first row the previous prompt sent.
    pub fn up_in(&mut self, width: u16) {
        let (_, (row, col)) = self.layout(width);
        if row == 0 {
            self.history_back();
            return;
        }
        self.cursor = self.offset_at(width, row - 1, col);
        self.snap(false);
    }

    /// Down in the prompt as it is shown at `width`, as `up_in`; from the
    /// last row the next prompt sent.
    pub fn down_in(&mut self, width: u16) {
        let (rows, (row, col)) = self.layout(width);
        if usize::from(row) + 1 >= rows.len() {
            self.history_forward();
            return;
        }
        self.cursor = self.offset_at(width, row + 1, col);
        self.snap(false);
    }

    /// Where the caret goes for column `col` of row `row` as `layout` lays
    /// the text out at `width`: the last place on that row at or before
    /// the column.
    fn offset_at(&self, width: u16, row: u16, col: u16) -> usize {
        let width = usize::from(width.max(2));
        let (mut r, mut c, mut at) = (0u16, 0usize, 0usize);
        let mut best = None;
        for ch in self.text.chars() {
            let w = ch.width().unwrap_or(0);
            if ch != '\n' && c + w > width {
                r += 1;
                c = 0;
            }
            if r == row && c <= usize::from(col) {
                best = Some(at);
            }
            if r > row {
                break;
            }
            at += ch.len_utf8();
            if ch == '\n' {
                r += 1;
                c = 0;
            } else {
                c += w;
            }
        }
        // The row's end: past its last character, unless the screen wrapped
        // it there, where the caret would show on the next row.
        if r == row && c <= usize::from(col) {
            best = Some(at);
        }
        best.unwrap_or(at)
    }

    fn history_back(&mut self) {
        let next = self.browsing.map_or(0, |i| i + 1);
        if next >= self.history.len() {
            return;
        }
        if self.browsing.is_none() {
            self.draft = std::mem::take(&mut self.text);
        }
        // Another prompt's text: a paste still being read goes to the
        // caret of whatever is shown when it arrives.
        self.marks.clear();
        self.browsing = Some(next);
        self.text = self.history[self.history.len() - 1 - next].clone();
        self.cursor = self.text.len();
    }

    fn history_forward(&mut self) {
        if self.browsing.is_some() {
            self.marks.clear();
        }
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

/// Text as the prompt takes it: carriage returns as newlines, tabs as four
/// spaces, no other control characters, so what is shown is what is sent.
fn clean(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n").replace('\t', "    ").chars().filter(|c| *c == '\n' || !c.is_control()).collect()
}

/// What a pasted image is in the prompt.
pub fn image_token(n: u32) -> String {
    format!("[Image #{n}]")
}

/// Every `[Image #N]` in `text`, with its byte range, in order.
pub fn image_tokens(text: &str) -> impl Iterator<Item = (Range<usize>, u32)> + '_ {
    const OPEN: &str = "[Image #";
    text.match_indices(OPEN).filter_map(move |(start, _)| {
        let rest = &text[start + OPEN.len()..];
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        let n: u32 = rest[..digits].parse().ok()?;
        rest[digits..].starts_with(']').then(|| (start..start + OPEN.len() + digits + 1, n))
    })
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
    fn up_and_down_go_by_the_rows_shown_wrapped_ones_too() {
        // "0123456789ab" at 5 columns: 01234 / 56789 / ab, after a new line.
        let mut e = typed("x\n0123456789ab");
        assert_eq!(e.layout(5).1, (3, 2));
        e.up_in(5);
        assert_eq!(e.layout(5).1, (2, 2), "the row the screen wrapped, not the line before");
        e.up_in(5);
        assert_eq!(e.layout(5).1, (1, 2));
        e.up_in(5);
        assert_eq!(e.layout(5).1, (0, 1), "a shorter row: its end");
        e.down_in(5);
        e.down_in(5);
        e.down_in(5);
        assert_eq!(e.layout(5).1, (3, 1));
        // From the first row, the history; with none, nothing moves.
        let mut e = typed("0123456789");
        e.home();
        e.up_in(5);
        assert_eq!(e.layout(5).1, (0, 0));
        // A full row's end is where the screen wraps it: the next row's start.
        let mut e = typed("0123456789");
        assert_eq!(e.layout(5).1, (2, 0));
        e.up_in(5);
        assert_eq!(e.layout(5).1, (1, 0));
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
    fn an_image_is_one_unit_to_the_caret() {
        let mut e = typed("see");
        e.insert_image(1);
        e.insert_str("and");
        assert_eq!(e.text(), "see [Image #1] and");
        e.word_left();
        e.left();
        e.left();
        assert_eq!(&e.text()[..e.cursor()], "see ", "left steps over the whole token");
        e.right();
        assert_eq!(&e.text()[..e.cursor()], "see [Image #1]", "and right over it again");
        e.backspace();
        assert_eq!(e.text(), "see  and", "backspace takes it whole");
        let mut e = typed("a ");
        e.insert_image(2);
        e.home();
        e.right();
        e.right();
        e.delete();
        assert_eq!(e.text(), "a  ", "delete takes it whole, and leaves the space after it");
        let mut e = typed("x ");
        e.insert_image(3);
        e.kill_word();
        assert_eq!(e.text(), "x ", "a word kill takes it whole");
        let mut e = typed("[Image #4] and\nmore text here");
        e.home();
        (0..5).for_each(|_| e.right());
        e.up();
        assert_eq!(e.cursor(), "[Image #4]".len(), "a line move never lands inside one");
        assert_eq!(image_tokens("[Image #1][Image #x] [Image #22]").map(|(_, n)| n).collect::<Vec<_>>(), [1, 22]);
    }

    #[test]
    fn a_paste_still_being_read_lands_where_it_was_asked_for() {
        let mut e = typed("compare  please");
        (0..7).for_each(|_| e.left());
        let first = e.mark();
        let second = e.mark();
        // Typed while both are read: after them, as it was typed after.
        e.insert_str("and");
        e.home();
        e.insert_str(">> ");
        e.place_image(Some(first), 1);
        e.place_image(Some(second), 2);
        assert_eq!(e.text(), ">> compare [Image #1] [Image #2] and please", "in the order asked for, ahead of what was typed since");
        assert_eq!(&e.text()[..e.cursor()], ">> ", "the caret where it was");
        let m = e.mark();
        e.kill_to_end();
        e.place_str(Some(m), "x\ty");
        assert_eq!(e.text(), ">> x    y");
        let m = e.mark();
        e.take();
        e.place_image(Some(m), 3);
        assert_eq!(e.text(), "[Image #3] ", "its prompt sent, at the caret of the next");
        // Text put back above the prompt moves a mark once, with the rest.
        let mut e = typed("look at ");
        let m = e.mark();
        e.restore("steer");
        e.place_image(Some(m), 4);
        assert_eq!(e.text(), "steer\nlook at [Image #4] ");
        let mut e = typed("draft");
        e.home();
        let m = e.mark();
        e.restore("steer");
        e.place_image(Some(m), 6);
        assert_eq!(e.text(), "steer\n[Image #6] draft", "a mark at the start stays before the draft");
        // And a prompt swapped for another from the history drops it.
        let mut e = Editor::new(None);
        e.insert_str("a much longer prompt sent before");
        e.take();
        e.up();
        let m = e.mark();
        e.down();
        e.place_image(Some(m), 5);
        assert_eq!(e.text(), "[Image #5] ");
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
