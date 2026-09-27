//! The terminal: an inline ratatui viewport at the bottom of the normal
//! screen, and everything finished written above it into the terminal's own
//! scrollback (R-TUI-1). No alternate screen, no mouse capture, no keyboard
//! protocol extensions: what a phone terminal, tmux or an SSH session does
//! not understand is never sent (R-TUI-3).
//!
//! Three rules keep scrollback exact:
//!
//! - **One write per frame, inside synchronized output.** Every byte ratatui
//!   produces goes into a frame buffer, and the frame reaches the terminal
//!   as one write bracketed by CSI ?2026h … CSI ?2026l, so a terminal that
//!   supports it never shows half a frame, and one that does not ignores
//!   the brackets. One frame is one bracket pair, which is what the redraw
//!   budget counts.
//! - **Lines enter scrollback exactly once, as text.** Finished lines are
//!   printed where the viewport was — styled text and a CR LF each, the way
//!   a shell command's output is — and the viewport is rebuilt below them;
//!   they are never redrawn afterwards. A line wider than the terminal is
//!   left for the terminal to wrap, so it rewraps itself when the window
//!   is resized, and copies as one line. (Grok Build's inline terminal,
//!   `xai-ratatui-inline`'s `emit_to_scrollback`, does the same; this is
//!   written from the idea, not its code.) The viewport is the only thing
//!   ever repainted.
//! - **Every move is relative to where the cursor is.** A frame already on
//!   its way when the terminal changes size is read by the terminal at the
//!   new size, where an absolute row names some other row — a clamped one,
//!   on a screen that got shorter — and the frame clears and draws in the
//!   wrong place, leaving the old live region in scrollback above the new
//!   one. The terminal keeps the cursor on the caret through a resize, so a
//!   frame that moves up and down from it still starts at the region's top
//!   and still leaves the cursor on the caret, whichever size it is read at.
//!   Its cells are drawn with autowrap off, so a row wider than the screen
//!   it is read at is cut rather than pushed onto the next row. What is left:
//!   such a row above the caret, cut, is measured after the resize as if the
//!   terminal had reflowed it, and the clear starts that many rows too high.
//!   The prompt box's top edge is always such a row, so a narrowing that
//!   lands while a frame is on its way can blank the conversation line
//!   right above the region.
//!   The row it is on is tracked, not re-read: a glyph the terminal draws
//!   wider than `unicode-width` says would put that off until the next
//!   resize or job stop asks the terminal again.
//! - **The terminal is asked where the cursor is only when nothing else is
//!   reading it**: at start, and after a resize, with the key reader stopped
//!   (the caller drops it). Everything in between — a taller or shorter live
//!   region above all — is computed from where the viewport already is. A
//!   screen-clearing resize, which ratatui does on a horizontal shrink,
//!   would erase the visible part of the conversation, so the resize is
//!   handled here instead: the old live region is cleared from its top down
//!   and the viewport rebuilt in place.

use crossterm::terminal::{Clear, ClearType as CtClear};
use crossterm::{queue, QueueableCommand};
use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use ratatui::text::Line;
use ratatui::{Terminal, TerminalOptions, Viewport};
use std::cell::RefCell;
use std::io::{self, Write};
use std::rc::Rc;

/// Begin and end synchronized update (DEC private mode 2026).
pub const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
pub const SYNC_END: &[u8] = b"\x1b[?2026l";
/// The window title before krowk's, saved on xterm's title stack at start
/// and put back when the TUI gives the terminal up (a terminal without the
/// stack ignores both).
pub const TITLE_SAVE: &[u8] = b"\x1b[22;2t";
pub const TITLE_RESTORE: &[u8] = b"\x1b[23;2t";
/// Autowrap off and on again (DECAWM), around the live region's cells.
const AUTOWRAP_OFF: &[u8] = b"\x1b[?7l";
const AUTOWRAP_ON: &[u8] = b"\x1b[?7h";

/// Where a frame's bytes collect until the frame is done, and the row they
/// leave the cursor on. ratatui flushes its writer after almost every
/// operation; here that is a no-op, so the frame leaves in one piece.
#[derive(Clone, Default)]
pub struct FrameBuf(Rc<RefCell<Buf>>);

#[derive(Default)]
struct Buf {
    bytes: Vec<u8>,
    /// The row the cursor is on once `bytes` are written.
    row: u16,
    /// The row the cursor is on with what has been sent: where it is when
    /// what was queued since is dropped.
    sent_row: u16,
}

impl Write for FrameBuf {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().bytes.extend_from_slice(b);
        Ok(b.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl FrameBuf {
    /// What is queued, to send: the cursor is where it leaves it.
    fn take(&self) -> Vec<u8> {
        let mut b = self.0.borrow_mut();
        b.sent_row = b.row;
        std::mem::take(&mut b.bytes)
    }

    /// What is queued, dropped: the cursor is where what was sent left it.
    fn discard(&self) {
        let mut b = self.0.borrow_mut();
        b.bytes.clear();
        b.row = b.sent_row;
    }

    fn row(&self) -> u16 {
        self.0.borrow().row
    }

    /// The terminal says, or a sequence just sent sets, where the cursor is.
    fn set_row(&self, row: u16) {
        let mut b = self.0.borrow_mut();
        b.row = row;
        if b.bytes.is_empty() {
            b.sent_row = row;
        }
    }

    /// To column `x` of row `y` by moves from the row the cursor is on:
    /// CR, then up or down, then right.
    fn goto(&self, x: u16, y: u16) -> io::Result<()> {
        let mut b = self.0.borrow_mut();
        let from = b.row;
        b.bytes.push(b'\r');
        match y.cmp(&from) {
            std::cmp::Ordering::Less => write!(b.bytes, "\x1b[{}A", from - y)?,
            std::cmp::Ordering::Greater => write!(b.bytes, "\x1b[{}B", y - from)?,
            std::cmp::Ordering::Equal => {}
        }
        if x > 0 {
            write!(b.bytes, "\x1b[{x}C")?;
        }
        b.row = y;
        Ok(())
    }

    /// `n` line feeds: down `n` rows, scrolling at the bottom of a screen
    /// `height` rows tall.
    fn feed(&self, n: u16, height: u16) {
        let mut b = self.0.borrow_mut();
        b.bytes.extend(std::iter::repeat_n(b'\n', usize::from(n)));
        b.row = b.row.saturating_add(n).min(height.saturating_sub(1));
    }
}

/// crossterm's backend writing into the frame buffer, with the two things
/// that would otherwise talk to the terminal answered from what is known:
/// its size (so ratatui never resizes behind our back) and the cursor (so
/// ratatui never queries it while the key reader owns the input). Cells are
/// drawn here rather than by crossterm, which moves to absolute rows, and
/// with `sgr`'s styles: blink, hidden and underline colours, which nothing
/// here uses, are not drawn. `scroll_region_*` and `clear` still go through
/// crossterm and set a scroll region, which moves the cursor where the row
/// model cannot follow; nothing calls them (ratatui's `insert_before` would).
pub struct Back {
    inner: CrosstermBackend<FrameBuf>,
    buf: FrameBuf,
    size: Size,
    cursor: Position,
}

impl Backend for Back {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let mut out = self.buf.clone();
        let mut last: Option<Position> = None;
        let mut style = None;
        for (x, y, cell) in content {
            if !matches!(last, Some(p) if x == p.x + 1 && y == p.y) {
                self.buf.goto(x, y)?;
            }
            last = Some(Position { x, y });
            let s = ratatui::style::Style::new().fg(cell.fg).bg(cell.bg).add_modifier(cell.modifier);
            if style != Some(s) {
                let codes = sgr(s);
                if codes.is_empty() {
                    out.write_all(b"\x1b[0m")?;
                } else {
                    write!(out, "\x1b[0;{codes}m")?;
                }
                style = Some(s);
            }
            out.write_all(cell.symbol().as_bytes())?;
        }
        if style.is_some() {
            out.write_all(b"\x1b[0m")?;
        }
        Ok(())
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.buf.feed(n, self.size.height);
        Ok(())
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        Ok(self.cursor)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let p = position.into();
        self.cursor = p;
        self.buf.goto(p.x, p.y)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> io::Result<Size> {
        Ok(self.size)
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        Ok(WindowSize { columns_rows: self.size, pixels: Size::default() })
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn scroll_region_up(&mut self, region: std::ops::Range<u16>, n: u16) -> io::Result<()> {
        self.inner.scroll_region_up(region, n)
    }

    fn scroll_region_down(&mut self, region: std::ops::Range<u16>, n: u16) -> io::Result<()> {
        self.inner.scroll_region_down(region, n)
    }
}

/// The live region and the scrollback above it.
pub struct Term<W: Write> {
    terminal: Terminal<Back>,
    buf: FrameBuf,
    out: W,
    size: Size,
    height: u16,
    /// The row the cursor was left on, relative to the viewport's top: the
    /// input caret. After a resize the terminal still knows where the cursor
    /// is, and the viewport's top is found from it.
    caret_row: u16,
    caret_col: u16,
    /// How many columns each live row last drawn actually used: after a
    /// narrowing resize, a terminal that reflows splits each wider row
    /// into several, and this is how many.
    widths: Vec<u16>,
    /// How wide the terminal was when the last frame reached it: what a
    /// resize is measured against, however many resizes come before the
    /// next frame.
    drawn_width: u16,
    /// Whether this terminal reflows lines on a narrowing resize. Most do;
    /// xterm and the Linux console truncate instead (see `reflows_from`).
    pub reflows: bool,
    /// Frames written, for the tests and the redraw budget's evidence.
    pub frames: u64,
    /// Columns kept clear on the left and the right of everything drawn.
    /// The left ones are moved over, never written — as Claude Code does —
    /// so a terminal that copies only what was written leaves them out.
    pub pad: u16,
}

impl<W: Write> Term<W> {
    /// A viewport `height` rows tall at the bottom of the screen. `top` is
    /// where the cursor was when the TUI started: what is above it on screen
    /// is moved down to sit right above the viewport (see `anchor`).
    pub fn new(mut out: W, size: Size, top: u16, height: u16) -> io::Result<Term<W>> {
        let buf = FrameBuf::default();
        buf.set_row(top);
        let height = height.clamp(1, size.height.max(1));
        let top = anchor(&buf, size, top, height)?;
        // Out now, not with the first frame: a resize before that frame
        // measures against a screen that has already moved.
        out.write_all(&buf.take())?;
        out.flush()?;
        let terminal = build(&buf, size, top, height)?;
        Ok(Term { terminal, buf, out, size, height, caret_row: 0, caret_col: 0, widths: Vec::new(), drawn_width: size.width, reflows: true, frames: 0, pad: 0 })
    }

    pub fn width(&self) -> u16 {
        self.size.width
    }

    pub fn size(&self) -> Size {
        self.size
    }

    fn top(&mut self) -> u16 {
        self.terminal.get_frame().area().y
    }

    /// Changes the live region's height. A taller one keeps its top and
    /// pushes what is above it up into scrollback the way a new line would;
    /// a shorter one is cleared and drops back to the bottom of the screen,
    /// what is above it moving down with it (`anchor`), so the prompt never
    /// floats with blank rows under it.
    fn set_height(&mut self, height: u16) -> io::Result<()> {
        let height = height.clamp(1, self.size.height.max(1));
        if height == self.height {
            return Ok(());
        }
        let mut top = self.top();
        if height < self.height {
            self.buf.goto(0, top)?;
            queue!(self.buf.clone(), Clear(CtClear::FromCursorDown))?;
            top = anchor(&self.buf, self.size, top, height)?;
        }
        self.rebuild(top, height)
    }

    fn rebuild(&mut self, top: u16, height: u16) -> io::Result<()> {
        self.buf.goto(0, top)?;
        queue!(self.buf.clone(), Clear(CtClear::FromCursorDown))?;
        self.terminal = build(&self.buf, self.size, top, height)?;
        self.height = height;
        Ok(())
    }

    /// The terminal changed size. `cursor_row` is where the terminal says
    /// the cursor is now, when it could be asked. The old live region is
    /// found from it and cleared from its top down, then drawn afresh.
    ///
    /// Where its top is depends on what the terminal did to it. One that
    /// truncates (xterm, the Linux console) leaves every row where it was, so
    /// the top is the cursor less the caret's row. One that reflows (tmux,
    /// kitty, VTE, iTerm, WezTerm, Windows Terminal) splits each row wider
    /// than the new width into several and keeps the cursor on the caret, so
    /// the top is the cursor less the rows the region above the caret now
    /// takes — cleared from there, the status bar, overlay or notice that
    /// was split leaves nothing behind.
    ///
    /// Everything is measured against the last frame the terminal actually
    /// got: bytes queued by an earlier resize and never flushed are dropped,
    /// so two resizes before a frame are one resize from what is on screen.
    ///
    /// A reflow never pushes the region into history, because the region
    /// sits at the bottom of the screen (`anchor`): tmux and the others keep
    /// the bottom of their grid on screen, so the rows a reflow adds push
    /// out what is above the region — conversation, already in scrollback's
    /// order — and never the region itself, unless the reflowed region is
    /// taller than the whole screen.
    pub fn resize(&mut self, size: Size, cursor_row: Option<u16>) -> io::Result<()> {
        self.buf.discard();
        self.size = size;
        let height = self.height.clamp(1, size.height.max(1));
        let narrowed = size.width < self.drawn_width;
        let above = if narrowed && self.reflows { self.reflowed_above_caret(size.width) } else { self.caret_row };
        // Not asked, the cursor is still on the caret, but its row was
        // numbered on the old screen: on the new one it is at most the last.
        let row = cursor_row.unwrap_or_else(|| self.buf.row().min(size.height.saturating_sub(1)));
        self.buf.set_row(row);
        let top = row.saturating_sub(above);
        // A shorter screen can leave the region's top too low for all of it:
        // the terminal took the rows below the caret. The rows it needs are
        // made the way a new line makes them, scrolling what is above into
        // scrollback — moving the top up instead would clear conversation.
        let top = top.min(size.height.saturating_sub(1));
        self.rebuild(top, height)
    }

    /// Rows the live region above the caret takes once reflowed to `width`.
    pub fn reflowed_above_caret(&self, width: u16) -> u16 {
        let width = width.max(1);
        let rows = |w: u16| w.div_ceil(width).max(1);
        let above: u16 = self.widths.iter().take(usize::from(self.caret_row)).map(|w| rows(*w)).sum();
        above + self.caret_col / width
    }

    /// Puts `text` on the clipboard with the next frame (OSC 52).
    pub fn clipboard(&mut self, text: &str) -> io::Result<()> {
        self.buf.clone().write_all(crate::clipboard::osc52(text).as_bytes())
    }

    /// Sets the window title with the next frame (OSC 2, which moves
    /// nothing).
    pub fn title(&mut self, title: &str) -> io::Result<()> {
        let title = crate::card::clean(title);
        write!(self.buf.clone(), "\x1b]2;{title}\x07")
    }

    /// One frame: `lines` into scrollback, then the live region redrawn as
    /// `rows` with the cursor at `caret` (column, row), all as one
    /// synchronized write.
    pub fn frame(&mut self, lines: &[Line<'static>], rows: &[Line<'static>], caret: (u16, u16)) -> io::Result<()> {
        let width = self.size.width;
        let height = (rows.len() as u16).clamp(1, self.size.height.max(1));
        if lines.is_empty() {
            self.set_height(height)?;
        } else {
            self.emit(lines, height)?;
        }
        let shown = usize::from(self.height);
        let pad = if width > 2 * self.pad + 10 { self.pad } else { 0 };
        let inner = width - 2 * pad;
        // The live region is drawn with autowrap off: a row wider than the
        // screen it is read at — a frame drawn before a narrowing — is cut
        // at the last column rather than wrapped onto the next row, or off
        // the bottom one, which would scroll the screen under the cursor.
        self.buf.clone().write_all(AUTOWRAP_OFF)?;
        let drawn = self.terminal.draw(|f| {
            let area = f.area();
            for (i, row) in rows.iter().enumerate().take(usize::from(area.height)) {
                f.buffer_mut().set_line(area.x + pad, area.y + i as u16, row, inner);
            }
            f.set_cursor_position((area.x + pad + caret.0.min(inner.saturating_sub(1)), area.y + caret.1.min(area.height.saturating_sub(1))));
        });
        self.buf.clone().write_all(AUTOWRAP_ON)?;
        drawn?;
        self.widths = rows.iter().take(shown).map(|r| (r.width() as u16).min(inner) + pad).collect();
        let top = self.top();
        if let Some(Position { x, y }) = completed_cursor(&mut self.terminal) {
            self.caret_row = y.saturating_sub(top);
            self.caret_col = x;
        }
        self.drawn_width = width;
        self.flush()
    }

    /// `lines` printed from the viewport's top down, over it, and a
    /// viewport `height` rows tall rebuilt right below them. Printing past
    /// the bottom row scrolls the screen, which is what moves the
    /// conversation into scrollback.
    fn emit(&mut self, lines: &[Line<'static>], height: u16) -> io::Result<()> {
        let (w, h) = (self.size.width, self.size.height.max(1));
        let top = self.top();
        let mut out = self.buf.clone();
        self.buf.goto(0, top)?;
        queue!(out, Clear(CtClear::FromCursorDown))?;
        let mut used: u32 = 0;
        let pad = if w > 2 * self.pad + 10 { self.pad } else { 0 };
        for line in lines {
            if pad > 0 && line.width() > 0 {
                write!(out, "\x1b[{pad}C")?;
            }
            write_styled(&mut out, line)?;
            out.write_all(b"\r\n")?;
            used += u32::from(soft_rows(line, w.saturating_sub(pad)));
        }
        // The cursor is on the row after the text, at most the last; line
        // feeds from there reserve the viewport, scrolling if they must.
        let below = (u32::from(top) + used).min(u32::from(h - 1)) as u16;
        self.buf.set_row(below);
        self.buf.feed(height - 1, h);
        let y = (below + height - 1).min(h - 1) - (height - 1);
        self.terminal = build(&self.buf, self.size, y, height)?;
        self.height = height;
        Ok(())
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

    /// Clears the live region and leaves the cursor at its top, at the
    /// start of a line, so the shell prompt that follows lands right under
    /// the conversation.
    pub fn finish(&mut self) -> io::Result<()> {
        let top = self.top();
        let mut w = self.buf.clone();
        self.buf.goto(0, top)?;
        w.queue(Clear(CtClear::FromCursorDown))?;
        w.queue(crossterm::cursor::Show)?;
        w.write_all(TITLE_RESTORE)?;
        self.flush()
    }

    /// Back after a job stop: the live region starts afresh on the row the
    /// cursor is on now (the shell may have printed below it).
    pub fn resume(&mut self, size: Size, cursor_row: Option<u16>) -> io::Result<()> {
        self.buf.discard();
        self.size = size;
        let height = self.height.clamp(1, size.height.max(1));
        // A cursor near the bottom keeps its row: the rebuild scrolls what
        // the shell printed up, rather than clearing it.
        // Unknown, the cursor is taken to be on the last row: the line
        // feeds that reserve the region only move down or scroll, so nothing
        // the shell printed is cleared wherever it really is.
        let top = cursor_row.unwrap_or(size.height.saturating_sub(1)).min(size.height.saturating_sub(1));
        self.buf.set_row(top);
        let top = anchor(&self.buf, size, top, height)?;
        self.rebuild(top, height)?;
        self.drawn_width = size.width;
        Ok(())
    }

    pub fn into_inner(self) -> W {
        self.out
    }
}

fn completed_cursor(t: &mut Terminal<Back>) -> Option<Position> {
    t.backend_mut().get_cursor_position().ok()
}

/// How many rows the terminal gives `line` at `width` columns when it wraps
/// it itself: a wide character that does not fit moves to the next row
/// whole, and an empty line is still a row.
pub fn soft_rows(line: &Line<'_>, width: u16) -> u16 {
    use unicode_width::UnicodeWidthChar;
    let width = usize::from(width.max(1));
    let (mut rows, mut col) = (1u16, 0usize);
    for c in line.spans.iter().flat_map(|s| s.content.chars()) {
        let cw = c.width().unwrap_or(0);
        if cw == 0 {
            continue;
        }
        if col + cw > width {
            rows = rows.saturating_add(1);
            col = 0;
        }
        col += cw;
    }
    rows
}

/// `line` as text with SGR styling, reset at its end.
///
/// Every span is cleaned on its way out, whoever built it: what reaches
/// scrollback is model output, tool names and paths, and none of it may
/// carry an escape sequence or a control character to the terminal.
fn write_styled(out: &mut impl Write, line: &Line<'_>) -> io::Result<()> {
    for span in &line.spans {
        let style = line.style.patch(span.style);
        let sgr = sgr(style);
        let text: String = span.content.chars().filter(|c| !c.is_control()).collect();
        // A URL is a hyperlink to itself (OSC 8), which a terminal that
        // does not know the sequence skips.
        if crate::look::is_link(span) {
            write!(out, "\x1b]8;;{text}\x1b\\\x1b[{sgr}m{text}\x1b[0m\x1b]8;;\x1b\\")?;
            continue;
        }
        if sgr.is_empty() {
            out.write_all(text.as_bytes())?;
        } else {
            write!(out, "\x1b[{sgr}m{text}\x1b[0m")?;
        }
    }
    Ok(())
}

fn sgr(style: ratatui::style::Style) -> String {
    use ratatui::style::{Color, Modifier};
    let mut codes: Vec<String> = Vec::new();
    let m = style.add_modifier;
    for (flag, code) in [(Modifier::BOLD, "1"), (Modifier::DIM, "2"), (Modifier::ITALIC, "3"), (Modifier::UNDERLINED, "4"), (Modifier::REVERSED, "7"), (Modifier::CROSSED_OUT, "9")] {
        if m.contains(flag) {
            codes.push(code.into());
        }
    }
    let colour = |c: Color, base: u8| -> Option<String> {
        let named = |n: u8| Some((base + n).to_string());
        match c {
            Color::Reset => None,
            Color::Black => named(0),
            Color::Red => named(1),
            Color::Green => named(2),
            Color::Yellow => named(3),
            Color::Blue => named(4),
            Color::Magenta => named(5),
            Color::Cyan => named(6),
            Color::Gray => named(7),
            Color::DarkGray => Some((base + 60).to_string()),
            Color::LightRed => Some((base + 61).to_string()),
            Color::LightGreen => Some((base + 62).to_string()),
            Color::LightYellow => Some((base + 63).to_string()),
            Color::LightBlue => Some((base + 64).to_string()),
            Color::LightMagenta => Some((base + 65).to_string()),
            Color::LightCyan => Some((base + 66).to_string()),
            Color::White => Some((base + 67).to_string()),
            Color::Indexed(i) => Some(format!("{};5;{i}", base + 8)),
            Color::Rgb(r, g, b) => Some(format!("{};2;{r};{g};{b}", base + 8)),
        }
    };
    if let Some(c) = style.fg.and_then(|c| colour(c, 30)) {
        codes.push(c);
    }
    if let Some(c) = style.bg.and_then(|c| colour(c, 40)) {
        codes.push(c);
    }
    codes.join(";")
}

/// Whether the terminal the environment names reflows on a narrowing
/// resize. Real xterm (which sets XTERM_VERSION) and the Linux console
/// truncate; everything else in use today reflows, tmux and screen included.
pub fn reflows_from(env: &dyn Fn(&str) -> String) -> bool {
    let inside_mux = !env("TMUX").is_empty() || env("TERM").starts_with("screen") || env("TERM").starts_with("tmux");
    inside_mux || !(env("TERM") == "linux" || !env("XTERM_VERSION").is_empty())
}

/// Moves the viewport to the bottom of the screen: the rows above `top`
/// (the shell's output, the command line) scroll down to sit right above
/// it, and the blank rows below the cursor become blank rows at the top of
/// the screen. Nothing on screen is lost or drawn twice — only blank rows
/// are scrolled out of the region. A region at the bottom is what keeps a
/// reflowing resize from pushing it into history (see `Term::resize`).
fn anchor(buf: &FrameBuf, size: Size, top: u16, height: u16) -> io::Result<u16> {
    let bottom = size.height.saturating_sub(height);
    // Already at the bottom, or below it: ratatui makes room by scrolling
    // the screen up, as a new line would.
    if top >= bottom {
        return Ok(top);
    }
    if top > 0 {
        CrosstermBackend::new(buf.clone()).scroll_region_down(0..bottom, bottom - top)?;
        // Setting the scroll region moves the cursor to the top left.
        buf.set_row(0);
    }
    Ok(bottom)
}

fn build(buf: &FrameBuf, size: Size, top: u16, height: u16) -> io::Result<Terminal<Back>> {
    // ratatui reserves the viewport's rows by printing newlines from the
    // cursor, so the real cursor has to be at the top first.
    buf.goto(0, top)?;
    let back = Back { inner: CrosstermBackend::new(buf.clone()), buf: buf.clone(), size, cursor: Position { x: 0, y: top } };
    Terminal::with_options(back, TerminalOptions { viewport: Viewport::Inline(height) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Span;

    fn frames(bytes: &[u8]) -> usize {
        bytes.windows(SYNC_BEGIN.len()).filter(|w| *w == SYNC_BEGIN).count()
    }

    #[test]
    fn r_tui_1_every_frame_is_one_synchronized_write() {
        let mut t = Term::new(Vec::new(), Size { width: 40, height: 10 }, 0, 3).unwrap();
        t.frame(&[Line::from("one"), Line::from("two")], &[Line::from("› hi"), Line::default(), Line::default()], (4, 0)).unwrap();
        t.frame(&[], &[Line::from("› hi there"), Line::default(), Line::default(), Line::default()], (10, 0)).unwrap();
        let out = t.into_inner();
        assert_eq!(frames(&out), 2, "one bracket pair per frame");
        let text = String::from_utf8_lossy(&out);
        assert!(text.starts_with("\x1b[?2026h") && text.ends_with("\x1b[?2026l"), "{text:?}");
        assert!(!text.contains("\x1b[2J"), "the screen is never cleared whole: {text:?}");
    }

    #[test]
    fn r_tui_3_a_frame_moves_only_up_and_down_from_the_cursor() {
        // Read at a size it was not written for, an absolute row lands
        // somewhere else; a move from the caret lands where it was meant to.
        let mut t = Term::new(Vec::new(), Size { width: 40, height: 10 }, 0, 3).unwrap();
        let start = t.out.len();
        t.frame(&[Line::from("one"), Line::from("two")], &[Line::from("› hi"), Line::from("bar"), Line::default()], (4, 0)).unwrap();
        t.frame(&[], &[Line::from("› hi there"), Line::from("bar"), Line::default()], (10, 0)).unwrap();
        t.resize(Size { width: 30, height: 8 }, Some(5)).unwrap();
        t.frame(&[Line::from("three")], &[Line::from("› hi there"), Line::from("bar"), Line::default()], (10, 0)).unwrap();
        let out = String::from_utf8_lossy(&t.out[start..]).into_owned();
        let absolute = out.split("\x1b[").skip(1).any(|seq| seq.find(|c: char| c.is_ascii_alphabetic()).is_some_and(|end| matches!(&seq[end..=end], "H" | "f" | "d" | "r")));
        assert!(!absolute, "an absolute move: {out:?}");
        assert!(out.contains("one\r\ntwo\r\n") && out.contains("three\r\n"), "{out:?}");
    }

    /// A 90-wide overlay row, the prompt with the caret at column 50, and a
    /// 90-wide status bar, on a 100x30 terminal whose cursor started at row 10.
    fn drawn() -> Term<Vec<u8>> {
        let mut t = Term::new(Vec::new(), Size { width: 100, height: 30 }, 10, 3).unwrap();
        let bar = Line::from("x".repeat(90));
        t.frame(&[], &[bar.clone(), Line::from("y".repeat(60)), bar], (50, 1)).unwrap();
        t
    }

    fn after_resize(t: &mut Term<Vec<u8>>, steps: &[(u16, u16)]) -> String {
        let before = t.out.len();
        for (w, row) in steps {
            t.resize(Size { width: *w, height: 30 }, Some(*row)).unwrap();
        }
        t.flush().unwrap();
        String::from_utf8_lossy(&t.out[before..]).into_owned()
    }

    #[test]
    fn r_tui_3_the_region_starts_at_the_bottom_with_what_was_above_it_moved_down() {
        let mut t = Term::new(Vec::new(), Size { width: 40, height: 10 }, 4, 3).unwrap();
        assert_eq!(t.top(), 7, "the bottom three rows");
        let out = String::from_utf8_lossy(&t.out).into_owned();
        // Rows 1-7 scrolled down by 3: the four rows above the cursor land
        // right above the viewport, and only blank rows leave the region.
        assert_eq!(out, "\x1b[1;7r\x1b[3T\x1b[r", "{out:?}");
        let t = Term::new(Vec::new(), Size { width: 40, height: 10 }, 0, 3).unwrap();
        assert!(t.out.is_empty(), "nothing above the cursor: nothing to move");
    }

    #[test]
    fn r_tui_3_a_narrowed_region_is_measured_as_the_terminal_reflows_it() {
        let mut t = drawn();
        assert_eq!(t.top(), 27);
        // At 40 columns the 90-wide row above the caret takes three rows,
        // and the caret itself has moved one row down its own line.
        assert_eq!(t.reflowed_above_caret(40), 3 + 1);
        assert_eq!(t.reflowed_above_caret(100), 1, "unchanged at the old width");
        // Reflowed at the bottom of the grid, the region is 3 + 2 + 3 rows,
        // 22 to 29, and the caret on row 26: its top is 26 - 4 = 22, four
        // rows up from the cursor.
        let out = after_resize(&mut t, &[(40, 26)]);
        assert!(out.starts_with("\x1b[?2026h\r\x1b[4A\x1b[J"), "{out:?}");
    }

    #[test]
    fn r_tui_3_a_terminal_that_truncates_keeps_the_region_where_it_was() {
        let mut t = drawn();
        t.reflows = false;
        let out = after_resize(&mut t, &[(40, 28)]);
        assert!(out.starts_with("\x1b[?2026h\r\x1b[1A\x1b[J"), "the caret's row, less one: {out:?}");
    }

    #[test]
    fn r_tui_3_two_resizes_before_a_frame_are_one_from_what_is_on_screen() {
        let mut t = drawn();
        // 100 -> 70 (the region reflows to 2 + 1 + 2 rows, caret on row 26),
        // then 70 -> 40 before any frame: measured from the frame drawn at
        // 100, and the first resize's clear is never sent.
        let out = after_resize(&mut t, &[(70, 26), (40, 26)]);
        assert_eq!(out.matches("\x1b[J").count(), 1, "one clear, not two: {out:?}");
        assert!(out.starts_with("\x1b[?2026h\r\x1b[4A\x1b[J"), "{out:?}");
    }

    #[test]
    fn nothing_reaches_scrollback_with_a_control_character_in_it() {
        // A tool name the model made up, escapes and all, as the tool line
        // builds it: into scrollback, it prints as text.
        let mut t = Term::new(Vec::new(), Size { width: 60, height: 10 }, 0, 2).unwrap();
        let evil = "evil\x1b]0;pwned\x07\x1b[2J\rname";
        let line = Line::from(vec![Span::styled("◆ ", ratatui::style::Style::new().fg(ratatui::style::Color::Green)), Span::raw(evil.to_string())]);
        t.frame(&[line], &[Line::from("❯ "), Line::default()], (2, 0)).unwrap();
        let out = String::from_utf8_lossy(&t.into_inner()).into_owned();
        assert!(out.contains("evil]0;pwned[2Jname"), "{out:?}");
        assert!(!out.contains("\x1b]0;") && !out.contains("\x07") && !out.contains("\x1b[2J"), "{out:?}");
    }

    #[test]
    fn only_xterm_and_the_console_are_taken_to_truncate() {
        let env = |pairs: &'static [(&'static str, &'static str)]| move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string()).unwrap_or_default();
        assert!(reflows_from(&env(&[("TERM", "xterm-256color")])), "gnome, alacritty and the rest say xterm too");
        assert!(!reflows_from(&env(&[("TERM", "xterm-256color"), ("XTERM_VERSION", "XTerm(390)")])));
        assert!(!reflows_from(&env(&[("TERM", "linux")])));
        assert!(reflows_from(&env(&[("TERM", "linux"), ("TMUX", "/tmp/tmux-1000/default,1,0")])), "tmux reflows whatever it runs in");
    }
}
