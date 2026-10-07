//! The fullscreen terminal (`tui.screen`: `fullscreen`, the default): the
//! alternate screen, the conversation kept here rather than in the
//! terminal's scrollback, and the live region — what streams, the prompt and
//! the status line — pinned to the bottom rows whatever is scrolled. The
//! terminal's own scrollback moves the whole screen, footer and all, so a
//! footer that stays put needs the scrolling done here.
//!
//! - **Scrolled here.** The mouse wheel (button reporting, SGR encoded) and
//!   PgUp/PgDn move the conversation above the live region; lines that
//!   arrive meanwhile leave the view where it is, and a dim row at its bottom
//!   says how much is below. Sending a prompt goes back to the bottom.
//! - **Selected here.** With the mouse reported, the terminal selects
//!   nothing itself (but on Shift-drag in most), so a drag over the
//!   conversation selects it here, shown reversed, and is copied when the
//!   button is let go; a line the screen wrapped copies as one. As Grok
//!   Build's fullscreen does.
//! - **One write per frame, inside synchronized output**, as inline.
//!   Moves are absolute: nothing is in a scrollback a resize could move.
//! - **Wrapped here.** A line wider than the screen is wrapped a grapheme at
//!   a time as the terminal would, and wrapped again on a resize.
//! - **Left to the shell's screen.** On the way out the alternate screen is
//!   left and the conversation printed onto the screen the shell had, the
//!   way inline mode leaves it in scrollback. A job stop or a vendor's login
//!   leaves the alternate screen, and coming back redraws it whole.

use crate::term::{Back, FrameBuf, AUTOWRAP_OFF, AUTOWRAP_ON, SYNC_BEGIN, SYNC_END, TITLE_RESTORE};
use ratatui::layout::{Rect, Size};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::{Terminal, TerminalOptions, Viewport};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

/// The alternate screen (with the cursor saved), the mouse's buttons, wheel
/// and drags reported in SGR's encoding (1002: motion only with a button
/// down — every move, 1003, would wake the loop for nothing), and the
/// keyboard protocol's level
/// pushed again (`term::KEYS_PUSH`): kitty and Ghostty keep a stack for each
/// screen, and shift-enter would be enter here. Bare, not bracketed by
/// DECSC/DECRC: xterm saves one cursor for both, and 1049 restores it on the
/// way out. And all of it given back.
pub const ENTER: &[u8] = b"\x1b[?1049h\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[>1u";
pub const LEAVE: &[u8] = b"\x1b[<u\x1b[?1006l\x1b[?1002l\x1b[?1000l\x1b[?1049l";
/// The cursor's blink off, and the terminal's own cursor back (see `term`).
const BLINK_OFF: &[u8] = b"\x1b[?12l";
const CURSOR_DEFAULT: &[u8] = b"\x1b[0 q";
/// Rows a notch of the wheel scrolls.
pub const WHEEL: isize = 3;

/// Whether the alternate screen is taken: what a panic's restore gives
/// back, and only then — leaving it when it was never entered restores a
/// cursor nobody saved.
static TAKEN: AtomicBool = AtomicBool::new(false);

/// Leaves the alternate screen if it is taken.
pub fn leave_if_taken(out: &mut impl Write) {
    if TAKEN.swap(false, Ordering::SeqCst) {
        let _ = out.write_all(LEAVE);
    }
}

pub struct Full<W: Write> {
    terminal: Terminal<Back>,
    buf: FrameBuf,
    out: W,
    size: Size,
    /// Every line printed, cleaned: what a resize wraps again and leaving
    /// prints.
    lines: Vec<Line<'static>>,
    /// `lines` wrapped to the screen's width.
    rows: Vec<Line<'static>>,
    /// For each of `rows`, whether it goes on the line above it: the screen
    /// wrapped it there.
    joined: Vec<bool>,
    /// The first of `rows` on screen, and how many are, in the last frame.
    top: usize,
    shown: usize,
    /// What is selected with the mouse: from the row and column pressed on
    /// to where it was dragged, and whether the button is still down.
    selection: Option<Selection>,
    /// Rows scrolled up from the bottom; none follows what arrives.
    scroll: usize,
    /// Rows the conversation had in the last frame: a page.
    view: u16,
    /// Where the last frame left the cursor.
    caret: Option<(u16, u16)>,
    /// Frames written, for the tests and the redraw budget's evidence.
    pub frames: u64,
    /// Columns kept clear on the left and the right of the live region.
    pub pad: u16,
    steady: bool,
    /// Whether this screen holds the alternate screen now.
    taken: bool,
}

impl<W: Write> Full<W> {
    /// Takes the alternate screen with the first frame.
    pub fn new(out: W, size: Size) -> io::Result<Full<W>> {
        let buf = FrameBuf::default();
        buf.clone().write_all(ENTER)?;
        TAKEN.store(true, Ordering::SeqCst);
        let terminal = Terminal::with_options(Back::fullscreen(&buf, size), TerminalOptions { viewport: Viewport::Fullscreen })?;
        Ok(Full { terminal, buf, out, size, lines: Vec::new(), rows: Vec::new(), joined: Vec::new(), top: 0, shown: 0, selection: None, scroll: 0, view: size.height, caret: None, frames: 0, pad: 0, steady: false, taken: true })
    }

    pub fn size(&self) -> Size {
        self.size
    }

    /// Scrolls the conversation `by` rows, up when positive.
    pub fn scroll(&mut self, by: isize) {
        let most = self.rows.len().saturating_sub(usize::from(self.view.saturating_sub(1)).max(1));
        self.scroll = self.scroll.saturating_add_signed(by).min(most);
    }

    /// A page of the conversation, less a row to keep a line in common.
    pub fn page(&self) -> isize {
        self.view.saturating_sub(2).max(1) as isize
    }

    /// Back to the bottom, following what arrives.
    pub fn follow(&mut self) {
        self.scroll = 0;
    }

    /// The row of `rows` at screen row `y`, if the conversation is there.
    fn at(&self, y: u16) -> Option<usize> {
        (usize::from(y) < self.shown).then(|| self.top + usize::from(y))
    }

    /// The left button pressed at (`x`, `y`): a selection starts there if it
    /// is on the conversation; any other one goes. True when it is.
    pub fn press(&mut self, x: u16, y: u16) -> bool {
        let had = self.selection.take().is_some();
        let Some(row) = self.at(y) else { return had };
        self.selection = Some(Selection { from: (row, x), to: (row, x), held: true });
        true
    }

    /// Dragged to (`x`, `y`): the selection follows, and the conversation
    /// scrolls under it at the top or bottom row. True when it moved.
    pub fn drag(&mut self, x: u16, y: u16) -> bool {
        if !self.selection.is_some_and(|s| s.held) || self.shown == 0 {
            return false;
        }
        let last = self.shown as u16 - 1;
        if y == 0 {
            self.scroll(1);
        } else if y >= last {
            self.scroll(-1);
        }
        let row = self.top + usize::from(y.min(last));
        if let Some(s) = self.selection.as_mut() {
            s.to = (row, x);
        }
        true
    }

    /// The button let go at (`x`, `y`): what is selected, as text to copy;
    /// none for a click that selected nothing, which leaves no selection.
    pub fn release(&mut self, x: u16, y: u16) -> Option<String> {
        self.drag(x, y);
        let s = self.selection.as_mut().filter(|s| s.held)?;
        s.held = false;
        if s.from == s.to {
            self.selection = None;
            return None;
        }
        let s = *s;
        Some(self.selected_text(s))
    }

    /// The text from `s`'s first cell to its last, a row the screen wrapped
    /// joined to the one above, a line's trailing blanks dropped.
    fn selected_text(&self, s: Selection) -> String {
        let ((r0, c0), (r1, c1)) = s.ordered();
        let mut text = String::new();
        for row in r0..=r1.min(self.rows.len().saturating_sub(1)) {
            if row > r0 && !self.joined[row] {
                let kept = text.trim_end_matches(' ').len();
                text.truncate(kept);
                text.push('\n');
            }
            let from = if row == r0 { c0 } else { 0 };
            let to = if row == r1 { c1.saturating_add(1) } else { u16::MAX };
            text.push_str(&cells(&self.rows[row], from, to));
        }
        let kept = text.trim_end_matches(' ').len();
        text.truncate(kept);
        text
    }

    pub fn clipboard(&mut self, text: &str) -> io::Result<()> {
        self.buf.clone().write_all(crate::clipboard::osc52(text).as_bytes())
    }

    pub fn title(&mut self, title: &str) -> io::Result<()> {
        let title = crate::card::clean(title);
        write!(self.buf.clone(), "\x1b]2;{title}\x07")
    }

    pub fn steady(&mut self, on: bool) -> io::Result<()> {
        if std::mem::replace(&mut self.steady, on) != on {
            self.buf.clone().write_all(if on { BLINK_OFF } else { CURSOR_DEFAULT })?;
        }
        Ok(())
    }

    /// One frame: `lines` added to the conversation, then the screen drawn
    /// — the conversation's rows above, `rows` at the bottom with the
    /// cursor at `caret` (column, row) — as one synchronized write. A frame
    /// that changes nothing on screen sends nothing.
    pub fn frame(&mut self, lines: &[Line<'static>], rows: &[Line<'static>], caret: (u16, u16)) -> io::Result<()> {
        let mark = self.buf.mark();
        for line in lines {
            self.push(line);
        }
        let (width, height) = (self.size.width, self.size.height);
        let live = (rows.len() as u16).min(height);
        self.view = height - live;
        let view = usize::from(self.view);
        // Scrolled, the view's last row says what is below it.
        self.scroll = self.scroll.min(self.rows.len().saturating_sub(view.saturating_sub(1).max(1)));
        let shown = if self.scroll > 0 { view.saturating_sub(1) } else { view };
        let end = self.rows.len() - self.scroll;
        let start = end.saturating_sub(shown);
        (self.top, self.shown) = (start, end - start);
        let selected = self.selection.map(Selection::ordered);
        let pad = if width > 2 * self.pad + 10 { self.pad } else { 0 };
        let inner = width - 2 * pad;
        let below = (self.scroll > 0 && view > 0).then(|| Line::from(Span::styled(format!("↓ {} more below · PgDn", self.scroll), crate::look::dim())));
        let at = (pad + caret.0.min(inner.saturating_sub(1)), self.view + caret.1.min(live.saturating_sub(1)));
        let conversation = &self.rows[start..end];
        self.buf.clone().write_all(AUTOWRAP_OFF)?;
        let drawn = self.terminal.draw(|f| {
            let b = f.buffer_mut();
            for (y, row) in conversation.iter().enumerate() {
                band(b, row, y as u16, 0, width);
                if let Some(((r0, c0), (r1, c1))) = selected
                    && (r0..=r1).contains(&(start + y))
                {
                    let from = if start + y == r0 { c0.min(width) } else { 0 };
                    let to = if start + y == r1 { c1.saturating_add(1).min(width) } else { width };
                    b.set_style(Rect::new(from, y as u16, to.saturating_sub(from), 1), Style::new().add_modifier(Modifier::REVERSED));
                }
            }
            if let Some(line) = &below {
                b.set_line(pad, self.view - 1, line, inner);
            }
            for (i, row) in rows.iter().take(usize::from(live)).enumerate() {
                let y = self.view + i as u16;
                if row.style.bg.is_some() {
                    b.set_style(Rect::new(0, y, width, 1), row.style);
                }
                b.set_line(pad, y, row, inner);
            }
            f.set_cursor_position(at);
        });
        self.buf.clone().write_all(AUTOWRAP_ON)?;
        drawn?;
        let drew = self.terminal.backend_mut().take_drew();
        if lines.is_empty() && !drew && self.caret == Some(at) {
            self.buf.rewind(mark);
        }
        self.caret = Some(at);
        self.flush()
    }

    /// `line`, cleaned, into the conversation, and its rows; a view scrolled
    /// up stays on what it shows.
    fn push(&mut self, line: &Line<'static>) {
        let line = clean(line);
        let rows = wrap(&line, self.size.width);
        if self.scroll > 0 {
            self.scroll += rows.len();
        }
        self.joined.extend((0..rows.len()).map(|i| i > 0));
        self.rows.extend(rows);
        self.lines.push(line);
    }

    /// The terminal changed size: the conversation wrapped again, the screen
    /// cleared and drawn whole by the next frame.
    pub fn resize(&mut self, size: Size) -> io::Result<()> {
        self.buf.discard();
        self.relayout(size)
    }

    fn relayout(&mut self, size: Size) -> io::Result<()> {
        if size.width != self.size.width {
            // Rows are numbered afresh: what was selected is not there.
            self.selection = None;
            (self.rows, self.joined) = (Vec::new(), Vec::new());
            for line in &self.lines {
                let rows = wrap(line, size.width);
                self.joined.extend((0..rows.len()).map(|i| i > 0));
                self.rows.extend(rows);
            }
        }
        self.size = size;
        self.terminal.backend_mut().set_size(size);
        self.terminal.resize(Rect::new(0, 0, size.width, size.height))?;
        self.caret = None;
        Ok(())
    }

    /// `/new`: the conversation gone, the screen cleared.
    pub fn wipe(&mut self) -> io::Result<()> {
        self.lines.clear();
        self.rows.clear();
        self.joined.clear();
        self.selection = None;
        self.scroll = 0;
        self.terminal.clear()?;
        self.caret = None;
        Ok(())
    }

    /// Gives the screen the shell had back: the cursor shown, its blink and
    /// the window title as they were.
    pub fn finish(&mut self) -> io::Result<()> {
        let mut w = self.buf.clone();
        crossterm::QueueableCommand::queue(&mut w, crossterm::cursor::Show)?;
        if std::mem::take(&mut self.steady) {
            w.write_all(CURSOR_DEFAULT)?;
        }
        w.write_all(TITLE_RESTORE)?;
        if std::mem::take(&mut self.taken) {
            w.write_all(LEAVE)?;
            TAKEN.store(false, Ordering::SeqCst);
        }
        self.flush()
    }

    /// The conversation, printed onto the shell's screen as inline mode
    /// leaves it in scrollback: styled text, the terminal wrapping it.
    pub fn print_conversation(&mut self) -> io::Result<()> {
        let width = self.size.width;
        let mut out = Vec::new();
        for line in &self.lines {
            crate::term::write_styled(&mut out, line)?;
            if let Some(bg) = line.style.bg
                && !crate::term::fills_last_row(line, width)
            {
                write!(out, "\x1b[{}m\x1b[K\x1b[0m", crate::term::sgr(Style::new().bg(bg)))?;
            }
            out.extend_from_slice(b"\r\n");
        }
        self.out.write_all(&out)?;
        self.out.flush()
    }

    /// Back after a job stop or a vendor's login: the alternate screen taken
    /// again at the size the window is now, and drawn whole.
    pub fn resume(&mut self, size: Size) -> io::Result<()> {
        self.buf.discard();
        self.buf.clone().write_all(ENTER)?;
        TAKEN.store(true, Ordering::SeqCst);
        self.taken = true;
        self.relayout(size)?;
        self.flush()
    }

    fn flush(&mut self) -> io::Result<()> {
        let body = self.buf.take();
        if body.is_empty() {
            return Ok(());
        }
        let mut frame = Vec::with_capacity(body.len() + SYNC_BEGIN.len() + SYNC_END.len());
        frame.extend_from_slice(SYNC_BEGIN);
        frame.extend_from_slice(&body);
        frame.extend_from_slice(SYNC_END);
        self.out.write_all(&frame)?;
        self.out.flush()?;
        self.frames += 1;
        Ok(())
    }

    pub fn into_inner(self) -> W {
        self.out
    }
}

/// A mouse selection over the conversation's rows: (row, column) pressed
/// on, and dragged to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Selection {
    from: (usize, u16),
    to: (usize, u16),
    held: bool,
}

impl Selection {
    /// Its first cell and its last, in reading order.
    fn ordered(self) -> ((usize, u16), (usize, u16)) {
        if self.from <= self.to { (self.from, self.to) } else { (self.to, self.from) }
    }
}

/// The text of `row`'s cells from column `from` up to `to`, a link's target
/// left out: a grapheme is in it when it starts there.
fn cells(row: &Line<'_>, from: u16, to: u16) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    let (from, to) = (usize::from(from), usize::from(to));
    let mut col = 0usize;
    let mut text = String::new();
    for g in row.spans.iter().flat_map(|s| s.content.graphemes(true)) {
        if (from..to).contains(&col) {
            text.push_str(&crate::look::untagged(g));
        }
        col += g.width().min(2);
    }
    text
}

/// A conversation row at `y`: a band (`Line::style`'s background) across
/// the whole width, as inline mode erases it to the edge.
fn band(b: &mut ratatui::buffer::Buffer, row: &Line<'_>, y: u16, x: u16, width: u16) {
    if row.style.bg.is_some() {
        b.set_style(Rect::new(0, y, width, 1), row.style);
    }
    b.set_line(x, y, row, width);
}

/// `line` with no control character in it — model output, tool names and
/// paths reach the screen as text — and its links one cell each, the way
/// the live region carries them (`look::cell_link`).
fn clean(line: &Line<'static>) -> Line<'static> {
    use unicode_segmentation::UnicodeSegmentation;
    let text = |s: &str| s.chars().filter(|c| !c.is_control()).collect::<String>();
    let mut spans = Vec::with_capacity(line.spans.len());
    for span in &line.spans {
        match crate::look::link_target(span) {
            Some((shown, url)) => spans.extend(text(shown).graphemes(true).map(|g| crate::look::linked(g.to_string(), span.style, &url))),
            None => spans.push(Span::styled(text(&span.content), span.style)),
        }
    }
    Line { spans, ..line.clone() }
}

/// `line` in rows of `width` columns, wrapped a grapheme at a time as the
/// terminal wraps it (`term::soft_rows`): a wide character that does not fit
/// moves to the next row whole, and an empty line is still a row.
fn wrap(line: &Line<'static>, width: u16) -> Vec<Line<'static>> {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    let width = usize::from(width.max(1));
    let mut rows = Vec::new();
    let mut row: Vec<Span<'static>> = Vec::new();
    let mut col = 0usize;
    for span in &line.spans {
        let mut piece = String::new();
        for g in span.content.graphemes(true) {
            let cw = g.width().min(2);
            if cw > 0 && col > 0 && col + cw > width {
                if !piece.is_empty() {
                    row.push(Span::styled(std::mem::take(&mut piece), span.style));
                }
                rows.push(Line { spans: std::mem::take(&mut row), ..line.clone() });
                col = 0;
            }
            piece.push_str(g);
            col += cw;
        }
        if !piece.is_empty() {
            row.push(Span::styled(piece, span.style));
        }
    }
    rows.push(Line { spans: row, ..line.clone() });
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(n: usize) -> Vec<Line<'static>> {
        (0..n).map(|i| Line::from(format!("line {i}"))).collect()
    }

    fn footer() -> Vec<Line<'static>> {
        vec![Line::from("› hi"), Line::from("status")]
    }

    /// The screen as text, row by row, from what was sent: the moves,
    /// clears and text a frame here is made of, trailing blanks trimmed.
    fn screen(out: &[u8], size: Size) -> Vec<String> {
        use unicode_width::UnicodeWidthChar;
        let (w, h) = (usize::from(size.width), usize::from(size.height));
        let mut grid = vec![vec![String::from(" "); w]; h];
        let (mut x, mut y) = (0usize, 0usize);
        let text = String::from_utf8_lossy(out);
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\x1b' => match chars.next() {
                    Some('[') => {
                        let mut params = String::new();
                        let fin = loop {
                            match chars.next() {
                                Some(c) if c.is_ascii_alphabetic() || c == '~' => break c,
                                Some(c) => params.push(c),
                                None => break ' ',
                            }
                        };
                        match fin {
                            'H' => {
                                let mut p = params.split(';').map(|n| n.parse::<usize>().unwrap_or(1));
                                y = p.next().unwrap_or(1) - 1;
                                x = p.next().unwrap_or(1) - 1;
                            }
                            'J' if params == "2" => grid = vec![vec![String::from(" "); w]; h],
                            _ => {}
                        }
                    }
                    // OSC, to BEL or ST.
                    Some(']') => {
                        while let Some(c) = chars.next() {
                            if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                                break;
                            }
                        }
                    }
                    _ => {}
                },
                '\r' => x = 0,
                '\n' => y += 1,
                c => {
                    let cw = c.width().unwrap_or(0);
                    if cw == 0 {
                        continue;
                    }
                    if y < h && x < w {
                        grid[y][x] = c.to_string();
                        if cw == 2 && x + 1 < w {
                            grid[y][x + 1] = String::new();
                        }
                    }
                    x += cw;
                }
            }
        }
        grid.into_iter().map(|r| r.concat().trim_end().to_string()).collect()
    }

    #[test]
    fn the_footer_is_on_the_bottom_rows_however_the_conversation_is_scrolled() {
        let size = Size { width: 30, height: 8 };
        let mut t = Full::new(Vec::new(), size).unwrap();
        t.frame(&lines(20), &footer(), (4, 0)).unwrap();
        let s = screen(&t.out, size);
        assert_eq!(s[..6], ["line 14", "line 15", "line 16", "line 17", "line 18", "line 19"].map(String::from), "{s:?}");
        assert_eq!(s[6..], ["› hi", "status"].map(String::from));
        t.scroll(t.page());
        t.frame(&[], &footer(), (4, 0)).unwrap();
        let s = screen(&t.out, size);
        assert_eq!((s[0].as_str(), s[4].as_str()), ("line 11", "line 15"), "a page up, two lines in common: {s:?}");
        assert!(s[5].starts_with("↓ 4 more below"), "{s:?}");
        assert_eq!(s[6..], ["› hi", "status"].map(String::from), "the footer where it was");
        // A line arriving leaves the view where it is.
        t.frame(&[Line::from("new")], &footer(), (4, 0)).unwrap();
        let s = screen(&t.out, size);
        assert_eq!(s[0], "line 11", "{s:?}");
        assert!(s[5].starts_with("↓ 5 more below"), "{s:?}");
        t.scroll(-100);
        t.frame(&[], &footer(), (4, 0)).unwrap();
        let s = screen(&t.out, size);
        assert_eq!(s[5], "new", "back at the bottom: {s:?}");
        t.scroll(1000);
        t.frame(&[], &footer(), (4, 0)).unwrap();
        assert_eq!(screen(&t.out, size)[0], "line 0", "no further than the top");
    }

    #[test]
    fn a_drag_over_the_conversation_selects_it_and_letting_go_copies_it_whole() {
        let size = Size { width: 10, height: 8 };
        let mut t = Full::new(Vec::new(), size).unwrap();
        // "0123456789abc" wraps to two rows; a link's target never copies.
        let mut fence = crate::look::Markdown::default();
        let link = crate::look::markdown_line("[docs](https://krowk.com/d)", &mut fence);
        t.frame(&[Line::from("one   "), Line::from("0123456789abc"), link], &footer(), (4, 0)).unwrap();
        assert!(!t.press(3, 6), "the footer is no conversation");
        assert!(t.press(1, 0));
        assert!(t.drag(3, 3));
        t.frame(&[], &footer(), (4, 0)).unwrap();
        let out = String::from_utf8_lossy(&t.out).into_owned();
        assert!(out.contains("\x1b[0;7mne"), "shown reversed: {out:?}");
        assert_eq!(t.release(3, 3).as_deref(), Some("ne\n0123456789abc\ndocs"), "the wrapped line as one, the trailing blanks dropped");
        assert!(!t.drag(5, 3), "let go: nothing follows the mouse");
        // A click selects nothing, and takes the selection away.
        assert!(t.press(2, 1));
        assert_eq!(t.release(2, 1), None);
        assert_eq!(t.selection, None);
        // Dragged upwards, it reads in order all the same.
        t.press(3, 1);
        assert_eq!(t.release(1, 0).as_deref(), Some("ne\n0123"));
    }

    #[test]
    fn a_short_conversation_is_at_the_top_and_the_footer_at_the_bottom() {
        let size = Size { width: 30, height: 8 };
        let mut t = Full::new(Vec::new(), size).unwrap();
        t.frame(&lines(2), &footer(), (4, 0)).unwrap();
        let s = screen(&t.out, size);
        assert_eq!((s[0].as_str(), s[1].as_str(), s[2].as_str(), s[6].as_str()), ("line 0", "line 1", "", "› hi"));
        let out = String::from_utf8_lossy(&t.out).into_owned();
        assert!(out.starts_with("\x1b[?2026h\x1b[?1049h\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[>1u"), "{out:?}");
        assert!(out.ends_with("\x1b[?2026l"), "one synchronized write: {out:?}");
    }

    #[test]
    fn a_wide_line_wraps_and_wraps_again_on_a_resize() {
        let size = Size { width: 10, height: 6 };
        let mut t = Full::new(Vec::new(), size).unwrap();
        t.frame(&[Line::from("0123456789abcde")], &footer(), (4, 0)).unwrap();
        let s = screen(&t.out, size);
        assert_eq!((s[0].as_str(), s[1].as_str()), ("0123456789", "abcde"));
        let size = Size { width: 20, height: 6 };
        t.resize(size).unwrap();
        t.frame(&[], &footer(), (4, 0)).unwrap();
        let s = screen(&t.out, size);
        assert_eq!((s[0].as_str(), s[1].as_str(), s[4].as_str()), ("0123456789abcde", "", "› hi"), "{s:?}");
    }

    #[test]
    fn a_frame_that_changes_nothing_sends_nothing() {
        let mut t = Full::new(Vec::new(), Size { width: 30, height: 8 }).unwrap();
        t.frame(&lines(3), &footer(), (4, 0)).unwrap();
        let (sent, frames) = (t.out.len(), t.frames);
        t.frame(&[], &footer(), (4, 0)).unwrap();
        assert_eq!((t.out.len(), t.frames), (sent, frames));
        t.frame(&[], &footer(), (3, 0)).unwrap();
        assert_eq!(t.frames, frames + 1, "a caret that moved is a change");
    }

    #[test]
    fn nothing_reaches_the_screen_with_a_control_character_in_it() {
        let mut t = Full::new(Vec::new(), Size { width: 60, height: 8 }).unwrap();
        t.frame(&[Line::from("evil\x1b]0;pwned\x07\x1b[2J\rname")], &footer(), (4, 0)).unwrap();
        t.finish().unwrap();
        t.print_conversation().unwrap();
        let out = String::from_utf8_lossy(&t.out).into_owned();
        assert!(!out.contains("\x1b]0;") && !out.contains('\x07') && !out.contains("\x1b[2J\r"), "{out:?}");
        assert!(out.contains("evil]0;pwned[2Jname"), "{out:?}");
    }

    #[test]
    fn a_link_is_a_hyperlink_cell_by_cell_and_printed_whole_on_the_way_out() {
        let mut t = Full::new(Vec::new(), Size { width: 60, height: 8 }).unwrap();
        let mut fence = crate::look::Markdown::default();
        t.frame(&[crate::look::markdown_line("see [docs](https://krowk.com/d)", &mut fence)], &footer(), (4, 0)).unwrap();
        let out = String::from_utf8_lossy(&t.out).into_owned();
        assert!(out.contains("\x1b]8;;https://krowk.com/d\x1b\\d\x1b]8;;\x1b\\"), "{out:?}");
        assert!(!out.chars().any(|c| ('\u{E0000}'..='\u{E007F}').contains(&c)), "the URL's carrier never reaches the terminal");
        let start = t.out.len();
        t.finish().unwrap();
        t.print_conversation().unwrap();
        let out = String::from_utf8_lossy(&t.out[start..]).into_owned();
        assert!(out.find("\x1b[?1049l").unwrap() < out.find("see").unwrap(), "printed on the shell's screen: {out:?}");
        assert!(!out.chars().any(|c| ('\u{E0000}'..='\u{E007F}').contains(&c)), "{out:?}");
    }

    #[test]
    fn the_alternate_screen_is_left_once() {
        let mut t = Full::new(Vec::new(), Size { width: 30, height: 8 }).unwrap();
        t.frame(&lines(1), &footer(), (4, 0)).unwrap();
        t.finish().unwrap();
        t.resume(Size { width: 30, height: 8 }).unwrap();
        t.frame(&[], &footer(), (4, 0)).unwrap();
        let s = screen(&t.out, Size { width: 30, height: 8 });
        assert_eq!(s[0], "line 0", "drawn whole again: {s:?}");
        t.finish().unwrap();
        let out = String::from_utf8_lossy(&t.out).into_owned();
        assert_eq!((out.matches("\x1b[?1049h").count(), out.matches("\x1b[?1049l").count()), (2, 2), "{out:?}");
    }
}
