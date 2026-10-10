//! The inline terminal (`tui.screen`: `inline`; fullscreen, the default, is
//! `crate::full`): a ratatui viewport at the bottom of the normal screen,
//! and everything finished written above it into the terminal's own
//! scrollback (R-TUI-1). No alternate screen, no mouse capture: what a phone
//! terminal, tmux or an SSH session does not understand is never sent
//! (R-TUI-3). The one exception is KEYS_PUSH, the keyboard protocol level
//! that tells shift-enter from enter: a terminal without it ignores it, and
//! one with it sends Ctrl, Alt and Esc keys in its own encoding, which
//! crossterm reads.
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
//!   The one exception is `wipe` (`/new`), which clears the screen and its
//!   scrollback from the home position: both mean the same at any size.
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
//! - **Printed text is never moved but by scrolling.** The region takes
//!   every row from under what is printed to the bottom of the screen, the
//!   prompt drawn at its bottom and the rows it does not need blank above
//!   it; lines printed fill them from the top, and past the bottom the
//!   screen scrolls up the way it does under a shell's output. Nothing is
//!   moved with insert or delete line: Ghostty (and herdr, built on it)
//!   forgets which rows of what they move were wrapped by the terminal, and
//!   a copy of a wrapped line then breaks at every row. A region that grows
//!   past the rows it has pushes what is above it up; one that shrinks
//!   cannot pull it back, and keeps its rows (`Term::set_height`).

use crossterm::QueueableCommand;
use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Rect, Size};
use ratatui::text::Line;
use ratatui::{Terminal, TerminalOptions, Viewport};
use std::cell::RefCell;
use std::collections::VecDeque;
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
/// The kitty keyboard protocol's first level (disambiguate), pushed on
/// the terminal's stack when the TUI takes the terminal and popped when it
/// gives it up: shift-enter then arrives apart from enter. Only that level:
/// text is still sent as text, and no key releases. Each is bracketed by
/// DECSC/DECRC: a terminal that reads a bare CSI u as restore-cursor (st)
/// restores the position just saved, and the cursor stays where it was.
pub const KEYS_PUSH: &[u8] = b"\x1b7\x1b[>1u\x1b8";
pub const KEYS_POP: &[u8] = b"\x1b7\x1b[<u\x1b8";
/// Autowrap off and on again (DECAWM), around the live region's cells.
pub(crate) const AUTOWRAP_OFF: &[u8] = b"\x1b[?7l";
pub(crate) const AUTOWRAP_ON: &[u8] = b"\x1b[?7h";

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
    pub(crate) fn take(&self) -> Vec<u8> {
        let mut b = self.0.borrow_mut();
        b.sent_row = b.row;
        std::mem::take(&mut b.bytes)
    }

    /// Where the queue stands: its length, and the row it leaves the cursor
    /// on, to go back to (`rewind`).
    pub(crate) fn mark(&self) -> (usize, u16) {
        let b = self.0.borrow();
        (b.bytes.len(), b.row)
    }

    /// What was queued since `mark`, dropped.
    pub(crate) fn rewind(&self, (len, row): (usize, u16)) {
        let mut b = self.0.borrow_mut();
        b.bytes.truncate(len);
        b.row = row;
    }

    /// What is queued, dropped: the cursor is where what was sent left it.
    pub(crate) fn discard(&self) {
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

    /// To column `x` of row `y`, absolutely: on the alternate screen, where
    /// nothing is in scrollback for a resize to move.
    fn cup(&self, x: u16, y: u16) -> io::Result<()> {
        let mut b = self.0.borrow_mut();
        write!(b.bytes, "\x1b[{};{}H", y + 1, x + 1)?;
        b.row = y;
        Ok(())
    }

    /// To the start of row `y` and clears from there to the end of the
    /// screen. From the top-left corner the clear is sent from the second
    /// column, and the first cleared on its own: a clear to the end of the
    /// screen from there is a clear of the whole screen to tmux, which
    /// scrolls what was on it into history first (`scroll-on-clear`) — a
    /// live region as tall as the screen would be left in scrollback.
    fn clear_down(&self, y: u16) -> io::Result<()> {
        self.goto(0, y)?;
        let mut b = self.0.borrow_mut();
        b.bytes.extend_from_slice(if y == 0 { b"\x1b[1C\x1b[J\x1b[1K\r" } else { b"\x1b[J" });
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
    /// Whether the last draw changed a cell.
    drew: bool,
    /// Whether the cursor was last shown: ratatui shows it every frame,
    /// and a terminal restarts its blink on every byte it is sent.
    shown: bool,
    /// Moves are absolute (`crate::full`'s alternate screen) rather than
    /// relative to the cursor.
    absolute: bool,
}

impl Back {
    /// A whole screen's backend, its moves absolute: `crate::full`'s.
    pub(crate) fn fullscreen(buf: &FrameBuf, size: Size) -> Back {
        Back { inner: CrosstermBackend::new(buf.clone()), buf: buf.clone(), size, cursor: Position::default(), drew: false, shown: false, absolute: true }
    }

    pub(crate) fn set_size(&mut self, size: Size) {
        self.size = size;
    }

    /// Whether the last draw changed a cell; cleared.
    pub(crate) fn take_drew(&mut self) -> bool {
        std::mem::take(&mut self.drew)
    }

    fn goto(&self, x: u16, y: u16) -> io::Result<()> {
        if self.absolute { self.buf.cup(x, y) } else { self.buf.goto(x, y) }
    }
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
            self.drew = true;
            if !matches!(last, Some(p) if x == p.x + 1 && y == p.y) {
                self.goto(x, y)?;
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
            // A cell a link is on is a hyperlink (OSC 8) of its own; the
            // terminal joins neighbours with the same URL into one.
            match crate::look::cell_link(cell.symbol()) {
                (text, Some(url)) => write!(out, "\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")?,
                (text, None) => out.write_all(text.as_bytes())?,
            }
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
        self.shown = false;
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        if std::mem::replace(&mut self.shown, true) {
            return Ok(());
        }
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        Ok(self.cursor)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let p = position.into();
        self.cursor = p;
        self.goto(p.x, p.y)
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
    /// The rows the last frame needed, of `height`: what a resize or a job
    /// stop reserves, rather than the spare rows above the prompt.
    want: u16,
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
    /// Columns kept clear on the left and the right of the live region.
    /// Scrollback has none: Ghostty, herdr and most terminals copy a
    /// column moved over as a space, so what is printed there starts at
    /// the first column and copies as it was written.
    pub pad: u16,
    /// Whether the cursor's blink is held off (`steady`).
    steady: bool,
    /// The last lines printed, newest last, at most `TAIL`: the rows right
    /// above the live region, what the full-height view (`cover`) draws
    /// over and puts back.
    /// Each with the width it was printed at: a terminal that does not
    /// reflow (`reflows`) keeps the rows it took then.
    tail: VecDeque<(Line<'static>, u16)>,
    /// While the view covers the screen: how many of `tail`'s last lines it
    /// drew over, to put back when it closes.
    covered: Option<usize>,
    /// Lines that reached the screen while the view covered it, printed
    /// once it closes: nothing goes into scrollback under it.
    held: Vec<Line<'static>>,
}

/// The most lines printed `tail` keeps: a line is at least a row, and no
/// screen is this tall.
const TAIL: usize = 512;

/// The cursor's blink off (DEC private mode 12), and the cursor the
/// terminal is set up with (DECSCUSR 0) — its own shape and blink, back.
const BLINK_OFF: &[u8] = b"\x1b[?12l";
const CURSOR_DEFAULT: &[u8] = b"\x1b[0 q";

impl<W: Write> Term<W> {
    /// A viewport from `top`, where the cursor was when the TUI started,
    /// under the shell's output, to the bottom of the screen, at least
    /// `height` rows tall.
    pub fn new(out: W, size: Size, top: u16, height: u16) -> io::Result<Term<W>> {
        let buf = FrameBuf::default();
        buf.set_row(top);
        let height = to_bottom(size, top, height);
        let terminal = build(&buf, size, top, height)?;
        Ok(Term { terminal, buf, out, size, height, want: height, caret_row: 0, caret_col: 0, widths: Vec::new(), drawn_width: size.width, reflows: true, frames: 0, pad: 0, steady: false, tail: VecDeque::new(), covered: None, held: Vec::new() })
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

    /// Makes the live region at least `want` rows tall. It reaches the
    /// bottom of the screen already, so a taller one pushes what is above
    /// it into scrollback the way a new line would. A shorter one keeps its
    /// rows, the ones it no longer needs blank above the prompt (`frame`
    /// draws at its bottom), and the next lines printed fill them: what was
    /// pushed cannot be pulled back, and moving what is above down to meet
    /// it would cost a copy its wraps.
    fn set_height(&mut self, want: u16) -> io::Result<()> {
        let want = want.clamp(1, self.size.height.max(1));
        if want <= self.height {
            return Ok(());
        }
        let top = self.top();
        self.rebuild(top, want)
    }

    fn rebuild(&mut self, top: u16, height: u16) -> io::Result<()> {
        self.buf.clear_down(top)?;
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
    /// A reflow never pushes the region into history: it is the last thing
    /// on screen, and tmux and the others keep the bottom of what is on
    /// their grid in view, so the rows a reflow adds push out what is above
    /// the region — conversation, already in scrollback's order — and never
    /// the region itself, unless the reflowed region is taller than the
    /// whole screen.
    pub fn resize(&mut self, size: Size, cursor_row: Option<u16>) -> io::Result<()> {
        self.buf.discard();
        self.size = size;
        // The view takes the whole screen, whatever its height now.
        let want = if self.covered.is_some() { size.height.max(1) } else { self.want.clamp(1, size.height.max(1)) };
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
        // Down to the bottom, the rows it does not need blank above the
        // prompt — but not when the cursor could not be asked: its row is a
        // guess, and rows reserved from a wrong one scroll conversation away.
        let height = if cursor_row.is_some() { to_bottom(size, top, want) } else { want };
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
    ///
    /// A frame that changes nothing on screen sends nothing: a terminal
    /// restarts the cursor's blink on whatever it is sent (Ghostty, at most
    /// every 500ms), so a no-op frame would only break the blink up.
    ///
    /// The first frame after the full-height view closes puts back what it
    /// drew over, then prints what arrived while it was open, then `lines`.
    pub fn frame(&mut self, lines: &[Line<'static>], rows: &[Line<'static>], caret: (u16, u16)) -> io::Result<()> {
        match self.uncover(lines) {
            Some(all) => self.paint(&all, rows, Some(caret)),
            None => {
                self.remember(lines);
                self.paint(lines, rows, Some(caret))
            }
        }
    }

    /// Closes the full-height view, if open: what it drew over, then what
    /// arrived while it was open, then `lines` — all to print, in order, and
    /// kept as printed. None when it was not open.
    fn uncover(&mut self, lines: &[Line<'static>]) -> Option<Vec<Line<'static>>> {
        let n = self.covered.take()?;
        let held = std::mem::take(&mut self.held);
        // Taken before the new ones are kept, which may let these go.
        let back = self.tail.len().saturating_sub(n);
        let mut all: Vec<Line<'static>> = self.tail.range(back..).map(|(l, _)| l.clone()).collect();
        all.extend(held.iter().cloned());
        all.extend(lines.iter().cloned());
        self.remember(&held);
        self.remember(lines);
        Some(all)
    }

    /// `lines`, printed at the width there is, kept as the last ones.
    fn remember(&mut self, lines: &[Line<'static>]) {
        let w = self.size.width;
        self.tail.extend(lines.iter().map(|l| (l.clone(), w)));
        let over = self.tail.len().saturating_sub(TAIL);
        self.tail.drain(..over);
    }

    /// Opens the full-height view (R-SUB-11): the live region becomes the
    /// whole screen, drawn by `view` until the next `frame` closes it. No
    /// alternate screen: it is the live region, drawn as every frame is.
    /// What it draws over is the conversation's last rows on screen — not
    /// scrollback — and the next `frame` prints them back where they were.
    /// Rows above them that were never printed here (the shell's, or the
    /// part of a wrapped line already scrolled off) cannot be put back:
    /// they are scrolled into scrollback first, as the next line would.
    /// A taller or shorter screen while it is open leaves nothing behind
    /// (`paint` keeps the hidden cursor on its top row). A narrower one that
    /// a reflowing terminal splits the view's rows for pushes as many of its
    /// top rows into scrollback: the limit every region taller than the
    /// screen has (`resize`), and there is no taking them back.
    pub fn cover(&mut self) -> io::Result<()> {
        if self.covered.is_some() {
            return Ok(());
        }
        let (w, h) = (self.size.width, self.size.height.max(1));
        let top = self.top();
        let (mut known, mut n) = (0u16, 0usize);
        for (line, printed) in self.tail.iter().rev() {
            // A terminal that reflows has rewrapped it to the width now; one
            // that does not left it in the rows it took when printed.
            let rows = soft_rows(line, if self.reflows { w } else { *printed });
            if known + rows > top {
                break;
            }
            known += rows;
            n += 1;
        }
        let unknown = top - known;
        if unknown > 0 {
            self.buf.goto(0, h - 1)?;
            self.buf.feed(unknown, h);
        }
        self.covered = Some(n);
        self.rebuild(0, h)
    }

    /// Whether the full-height view covers the screen.
    pub fn covered(&self) -> bool {
        self.covered.is_some()
    }

    /// A frame of the full-height view: `rows`, as many as the screen is
    /// tall, the cursor hidden. `lines` are held, printed once it closes.
    pub fn view(&mut self, lines: &[Line<'static>], rows: &[Line<'static>]) -> io::Result<()> {
        self.cover()?;
        self.held.extend(lines.iter().cloned());
        self.paint(&[], rows, None)
    }

    fn paint(&mut self, lines: &[Line<'static>], rows: &[Line<'static>], caret: Option<(u16, u16)>) -> io::Result<()> {
        let mark = self.buf.mark();
        let before = (self.height, self.caret_row, self.caret_col);
        let width = self.size.width;
        let height = (rows.len() as u16).clamp(1, self.size.height.max(1));
        self.want = height;
        if lines.is_empty() {
            self.set_height(height)?;
        } else {
            self.emit(lines, height)?;
        }
        let shown = usize::from(self.height);
        // Rows the region kept but does not need are blank above the rest.
        let spare = self.height.saturating_sub(rows.len() as u16);
        let pad = if width > 2 * self.pad + 10 { self.pad } else { 0 };
        let inner = width - 2 * pad;
        // The live region is drawn with autowrap off: a row wider than the
        // screen it is read at — a frame drawn before a narrowing — is cut
        // at the last column rather than wrapped onto the next row, or off
        // the bottom one, which would scroll the screen under the cursor.
        self.buf.clone().write_all(AUTOWRAP_OFF)?;
        self.terminal.backend_mut().drew = false;
        let drawn = self.terminal.draw(|f| {
            let area = f.area();
            for (i, row) in rows.iter().enumerate().take(usize::from(area.height)) {
                let y = area.y + spare + i as u16;
                // A row with a background of its own, the prompt's band,
                // has it across the whole screen, its text where it was.
                if row.style.bg.is_some() {
                    f.buffer_mut().set_style(Rect::new(area.x, y, width, 1), row.style);
                }
                f.buffer_mut().set_line(area.x + pad, y, row, inner);
            }
            // No caret, ratatui hides the cursor.
            if let Some(caret) = caret {
                f.set_cursor_position((area.x + pad + caret.0.min(inner.saturating_sub(1)), area.y + (spare + caret.1).min(area.height.saturating_sub(1))));
            }
        });
        self.buf.clone().write_all(AUTOWRAP_ON)?;
        drawn?;
        // Hidden, the cursor is left at the start of the region's top row:
        // a screen made shorter takes the rows below the cursor first (tmux
        // does, whatever is on them), so none of the view's is pushed into
        // scrollback, and a resize finds the top where the cursor is.
        if caret.is_none() {
            let top = self.top();
            self.buf.goto(0, top)?;
            self.terminal.backend_mut().cursor = Position { x: 0, y: top };
        }
        self.widths = std::iter::repeat_n(0, usize::from(spare)).chain(rows.iter().map(|r| if r.style.bg.is_some() { width } else { (r.width() as u16).min(inner) + pad })).take(shown).collect();
        let top = self.top();
        if let Some(Position { x, y }) = completed_cursor(&mut self.terminal) {
            self.caret_row = y.saturating_sub(top);
            self.caret_col = x;
        }
        self.drawn_width = width;
        if lines.is_empty() && !self.terminal.backend_mut().drew && before == (self.height, self.caret_row, self.caret_col) {
            self.buf.rewind(mark);
        }
        self.flush()
    }

    /// Holds the cursor's blink off, or gives the terminal's own cursor
    /// back. While a turn runs the region is redrawn every spinner frame,
    /// and a terminal that restarts the blink on output shows it as a
    /// flicker rather than a blink; held steady, it is a plain block.
    pub fn steady(&mut self, on: bool) -> io::Result<()> {
        if std::mem::replace(&mut self.steady, on) != on {
            self.buf.clone().write_all(if on { BLINK_OFF } else { CURSOR_DEFAULT })?;
        }
        Ok(())
    }

    /// `lines` printed from the viewport's top down, over it, and a
    /// viewport for `want` rows rebuilt right below them. Printing past
    /// the bottom row scrolls the screen, which is what moves the
    /// conversation into scrollback. Lines that leave room below them take
    /// it the way a shorter region does (`set_height`): the region is as
    /// tall as it needs while the session is all on screen, and otherwise
    /// reaches down to the bottom, the rows it does not need blank above the
    /// prompt.
    fn emit(&mut self, lines: &[Line<'static>], want: u16) -> io::Result<()> {
        let (w, h) = (self.size.width, self.size.height.max(1));
        let top = self.top();
        let mut out = self.buf.clone();
        self.buf.clear_down(top)?;
        let mut used: u32 = 0;
        for line in lines {
            write_styled(&mut out, line)?;
            // A band (`Line::style`'s background) is painted to the right
            // edge by erasing in its colour, not by spaces, so a copy of the
            // row ends where its text does. Not on a row the text filled:
            // the cursor waits on its last cell, which the erase would take.
            if let Some(bg) = line.style.bg
                && !fills_last_row(line, w)
            {
                write!(out, "\x1b[{}m\x1b[K\x1b[0m", sgr(ratatui::style::Style::new().bg(bg)))?;
            }
            out.write_all(b"\r\n")?;
            used += u32::from(soft_rows(line, w));
        }
        let end = u32::from(top) + used;
        // Rows the text and the region scroll off the top.
        let scrolled = (end + u32::from(want)).saturating_sub(u32::from(h));
        if scrolled == 0 {
            // Both fit: the region is every row under the text.
            let end = end as u16;
            self.buf.set_row(end);
            let height = h - end;
            self.terminal = build(&self.buf, self.size, end, height)?;
            self.height = height;
            return Ok(());
        }
        // The cursor is on the row after the text, at most the last; line
        // feeds from there reserve the viewport, scrolling as they must.
        self.buf.set_row(end.min(u32::from(h - 1)) as u16);
        self.buf.feed(want - 1, h);
        self.terminal = build(&self.buf, self.size, h - want, want)?;
        self.height = want;
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

    /// Clears the screen and its scrollback (`ESC [3J`, where the terminal
    /// has it) and starts the live region afresh on the top row: the lines
    /// the next frame prints go from there down, scrolling nothing into the
    /// history just cleared, the region under them, as on a screen `new`
    /// opened on.
    pub fn wipe(&mut self) -> io::Result<()> {
        self.forget();
        self.buf.clone().write_all(b"\x1b[H\x1b[2J\x1b[3J")?;
        self.buf.set_row(0);
        self.rebuild(0, to_bottom(self.size, 0, self.height))
    }

    /// Clears the live region and leaves the cursor at its top, at the
    /// start of a line, so the shell prompt that follows lands right under
    /// the conversation.
    ///
    /// With the full-height view open, what it drew over and what arrived
    /// meanwhile are printed first: leaving krowk, or a job stop, puts the
    /// conversation back as closing the view would.
    pub fn finish(&mut self) -> io::Result<()> {
        if let Some(all) = self.uncover(&[])
            && !all.is_empty()
        {
            self.emit(&all, 1)?;
        }
        let top = self.top();
        let mut w = self.buf.clone();
        self.buf.clear_down(top)?;
        w.queue(crossterm::cursor::Show)?;
        if std::mem::take(&mut self.steady) {
            w.write_all(CURSOR_DEFAULT)?;
        }
        w.write_all(TITLE_RESTORE)?;
        self.flush()
    }

    /// Back after a job stop: the live region starts afresh on the row the
    /// cursor is on now (the shell may have printed below it).
    pub fn resume(&mut self, size: Size, cursor_row: Option<u16>) -> io::Result<()> {
        self.buf.discard();
        // The shell may have printed above the region.
        self.forget();
        self.size = size;
        // The rows the prompt needs, not the spare ones it kept: those would
        // scroll the shell's output up.
        let height = self.want.clamp(1, size.height.max(1));
        // A cursor near the bottom keeps its row: the rebuild scrolls what
        // the shell printed up, rather than clearing it.
        // Unknown, the cursor is taken to be on the last row: the line
        // feeds that reserve the region only move down or scroll, so nothing
        // the shell printed is cleared wherever it really is.
        let top = cursor_row.unwrap_or(size.height.saturating_sub(1)).min(size.height.saturating_sub(1));
        self.buf.set_row(top);
        self.rebuild(top, to_bottom(size, top, height))?;
        self.drawn_width = size.width;
        // The cursor is where reserving the region left it until the next
        // frame puts it on the caret: a resize before then finds the top
        // from there.
        let top = self.top();
        (self.caret_row, self.caret_col) = (self.buf.row().saturating_sub(top), 0);
        self.widths.clear();
        // Out now, as at start: a resize before the next frame measures
        // against a screen that has already moved.
        self.flush()
    }

    /// What is above the region is no longer what was printed here: the
    /// view, if open, has nothing to put back.
    fn forget(&mut self) {
        self.tail.clear();
        if self.covered.is_some() {
            self.covered = Some(0);
        }
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
    soft_wrap(line, width).0
}

/// Whether `line` ends on the last column at `width` columns, the cursor
/// left waiting there for the terminal's next wrap.
pub(crate) fn fills_last_row(line: &Line<'_>, width: u16) -> bool {
    soft_wrap(line, width).1 == usize::from(width.max(1))
}

/// The rows `line` takes and the column it ends on, measured a grapheme at
/// a time as the terminal draws it: an emoji with its variation selector,
/// or several joined, is one glyph two columns wide.
fn soft_wrap(line: &Line<'_>, width: u16) -> (u16, usize) {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    let width = usize::from(width.max(1));
    let (mut rows, mut col) = (1u16, 0usize);
    for g in line.spans.iter().flat_map(|s| s.content.graphemes(true)) {
        let cw = g.width().min(2);
        if cw == 0 {
            continue;
        }
        if col + cw > width {
            rows = rows.saturating_add(1);
            col = 0;
        }
        col += cw;
    }
    (rows, col)
}

/// `line` as text with SGR styling, reset at its end.
///
/// Every span is cleaned on its way out, whoever built it: what reaches
/// scrollback is model output, tool names and paths, and none of it may
/// carry an escape sequence or a control character to the terminal.
pub(crate) fn write_styled(out: &mut impl Write, line: &Line<'_>) -> io::Result<()> {
    for span in &line.spans {
        let style = line.style.patch(span.style);
        let sgr = sgr(style);
        // A link is a hyperlink (OSC 8) the terminal opens, on a Ctrl-click
        // in most; a terminal that does not know the sequence skips it.
        if let Some((text, url)) = crate::look::link_target(span) {
            let clean = |s: &str| s.chars().filter(|c| !c.is_control()).collect::<String>();
            write!(out, "\x1b]8;;{}\x1b\\\x1b[{sgr}m{}\x1b[0m\x1b]8;;\x1b\\", clean(&url), clean(text))?;
            continue;
        }
        let text: String = span.content.chars().filter(|c| !c.is_control()).collect();
        if sgr.is_empty() {
            out.write_all(text.as_bytes())?;
        } else {
            write!(out, "\x1b[{sgr}m{text}\x1b[0m")?;
        }
    }
    Ok(())
}

pub(crate) fn sgr(style: ratatui::style::Style) -> String {
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

/// A region's height from `top`: down to the bottom of the screen, and at
/// least `height` rows, the screen scrolling to make them.
fn to_bottom(size: Size, top: u16, height: u16) -> u16 {
    height.max(size.height.saturating_sub(top)).clamp(1, size.height.max(1))
}

fn build(buf: &FrameBuf, size: Size, top: u16, height: u16) -> io::Result<Terminal<Back>> {
    // ratatui reserves the viewport's rows by printing newlines from the
    // cursor, so the real cursor has to be at the top first.
    buf.goto(0, top)?;
    let back = Back { inner: CrosstermBackend::new(buf.clone()), buf: buf.clone(), size, cursor: Position { x: 0, y: top }, drew: false, shown: false, absolute: false };
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
    fn a_wipe_prints_from_the_top_of_the_cleared_screen_and_scrolls_nothing() {
        let (w, h) = (40, 10);
        let mut t = Term::new(Vec::new(), Size { width: w, height: h }, 0, 3).unwrap();
        let many: Vec<Line<'static>> = (0..30).map(|i| Line::from(format!("line {i}"))).collect();
        let rows = [Line::from("› hi"), Line::default(), Line::default()];
        t.frame(&many, &rows, (4, 0)).unwrap();
        let start = t.out.len();
        t.wipe().unwrap();
        t.frame(&[Line::from("logo"), Line::from("Directory: ~")], &rows, (4, 0)).unwrap();
        let out = String::from_utf8_lossy(&t.out[start..]).into_owned();
        assert_eq!(frames(&t.out[start..]), 1, "the wipe goes with the frame: {out:?}");
        assert!(out.contains("\x1b[H\x1b[2J\x1b[3J"), "{out:?}");
        let after = &out[out.find("\x1b[3J").unwrap()..];
        assert!(after.find("logo").unwrap() < after.find("Directory").unwrap());
        assert_eq!((t.top(), t.height), (2, h - 2), "the region from right under the header to the bottom, nothing scrolled away");
    }

    #[test]
    fn scrollback_copies_as_written_from_the_first_column_its_bands_erased_to_the_edge() {
        use ratatui::style::{Color, Style};
        let mut t = Term::new(Vec::new(), Size { width: 10, height: 10 }, 0, 1).unwrap();
        t.pad = 2;
        let start = t.out.len();
        let band = Style::new().bg(Color::Indexed(235));
        t.frame(&[Line::from("plain"), Line::from("code").style(band), Line::from("").style(band), Line::from("0123456789").style(band)], &[Line::from("→")], (2, 0)).unwrap();
        let out = String::from_utf8_lossy(&t.out[start..]).into_owned();
        let at = |s: &str| out.find(s).unwrap_or_else(|| panic!("{s:?} in {out:?}"));
        assert!(out[..at("plain")].ends_with('\r'), "no column moved over before it: {out:?}");
        assert!(!out[at("plain")..at("→")].contains(' '), "no space before, inside or after a band: {out:?}");
        assert_eq!(out.matches("\x1b[48;5;235m\x1b[K").count(), 2, "the short row and the empty one erased in the band's colour: {out:?}");
        assert!(out[at("0123456789")..].starts_with("0123456789\x1b[0m\r\n"), "a full row is not, which would take its last cell: {out:?}");
    }

    #[test]
    fn a_band_row_an_emoji_fills_is_not_erased_into() {
        use ratatui::style::{Color, Style};
        // ❤️ is a heart and a variation selector: one glyph, two columns.
        let mut t = Term::new(Vec::new(), Size { width: 10, height: 10 }, 0, 1).unwrap();
        let start = t.out.len();
        t.frame(&[Line::from("abcdefgh\u{2764}\u{fe0f}").style(Style::new().bg(Color::Indexed(235)))], &[Line::from("→")], (2, 0)).unwrap();
        let out = String::from_utf8_lossy(&t.out[start..]).into_owned();
        assert!(!out.contains("\x1b[K"), "the row is full, the cursor on its last cell: {out:?}");
        assert_eq!(t.top(), 1, "one row");
    }

    #[test]
    fn a_narrowed_region_counts_its_spare_rows_above_the_prompt() {
        // From row 10 of 30 the region is twenty rows, seventeen of them
        // blank above the three it draws.
        let mut t = Term::new(Vec::new(), Size { width: 100, height: 30 }, 10, 3).unwrap();
        let bar = Line::from("x".repeat(90));
        t.frame(&[], &[bar.clone(), Line::from("y".repeat(60)), bar], (50, 1)).unwrap();
        assert_eq!((t.top(), t.height, t.caret_row), (10, 20, 18));
        // At 40 columns: 17 blank rows, 3 for the 90-wide one, 1 for the
        // caret down its own line.
        assert_eq!(t.reflowed_above_caret(40), 17 + 3 + 1);
        let out = after_resize(&mut t, &[(40, 28)]);
        assert!(out.starts_with("\x1b[?2026h\r\x1b[21A\x1b[J"), "{out:?}");
    }

    #[test]
    fn a_row_with_a_background_of_its_own_has_it_across_the_pad() {
        use ratatui::style::{Color, Style};
        let mut t = Term::new(Vec::new(), Size { width: 20, height: 10 }, 0, 2).unwrap();
        t.pad = 2;
        let start = t.out.len();
        let band = Style::new().bg(Color::Indexed(236));
        t.frame(&[], &[Line::from("→ hi").style(band), Line::from("status")], (4, 0)).unwrap();
        let out = String::from_utf8_lossy(&t.out[start..]).into_owned();
        let painted = out.split("48;5;236m").skip(1).map(|s| s.split(['\x1b', '\r']).next().unwrap_or("")).collect::<String>();
        assert_eq!(painted, format!("  → hi{}", " ".repeat(14)), "the band from the first column to the last, the text where the pad puts it: {out:?}");
        assert!(out.contains("status"), "{out:?}");
        assert!(t.widths.ends_with(&[20, 8]), "the band row as wide as the screen: {:?}", t.widths);
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

    #[test]
    fn a_frame_that_changes_nothing_sends_nothing_and_the_cursor_is_shown_once() {
        let mut t = Term::new(Vec::new(), Size { width: 40, height: 10 }, 0, 3).unwrap();
        let rows = |s: &str| vec![Line::from(s.to_string()), Line::from("› hi"), Line::default()];
        t.frame(&[], &rows("⠋ Working"), (4, 1)).unwrap();
        let (sent, frames) = (t.out.len(), t.frames);
        t.frame(&[], &rows("⠋ Working"), (4, 1)).unwrap();
        assert_eq!((t.out.len(), t.frames), (sent, frames), "{:?}", String::from_utf8_lossy(&t.out[sent..]));
        t.frame(&[], &rows("⠙ Working"), (4, 1)).unwrap();
        let spin = String::from_utf8_lossy(&t.out[sent..]).into_owned();
        assert!(spin.contains('⠙') && !spin.contains("\x1b[?25h"), "only the cell, the cursor already shown: {spin:?}");
        t.frame(&[], &rows("⠙ Working"), (3, 1)).unwrap();
        assert!(t.frames > frames + 1, "a caret that moved is a change");
    }

    #[test]
    fn the_blink_is_held_off_only_while_asked_and_the_terminal_s_cursor_comes_back() {
        let mut t = Term::new(Vec::new(), Size { width: 40, height: 10 }, 0, 3).unwrap();
        let rows = [Line::from("› hi")];
        t.steady(true).unwrap();
        t.frame(&[], &rows, (4, 0)).unwrap();
        t.steady(true).unwrap();
        t.frame(&[], &rows, (4, 0)).unwrap();
        t.steady(false).unwrap();
        t.frame(&[], &rows, (4, 0)).unwrap();
        t.steady(true).unwrap();
        t.finish().unwrap();
        let out = String::from_utf8_lossy(&t.out).into_owned();
        assert_eq!(out.matches("\x1b[?12l").count(), 2, "{out:?}");
        assert_eq!(out.matches("\x1b[0 q").count(), 2, "once let go, once on the way out: {out:?}");
    }

    /// A 90-wide overlay row, the prompt with the caret at column 50, and a
    /// 90-wide status bar, on a 100x30 terminal whose cursor started at row 10.
    fn drawn() -> Term<Vec<u8>> {
        let mut t = Term::new(Vec::new(), Size { width: 100, height: 30 }, 27, 3).unwrap();
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
    fn r_tui_3_the_region_starts_where_the_cursor_is_and_moves_nothing_above_it() {
        let mut t = Term::new(Vec::new(), Size { width: 40, height: 10 }, 4, 3).unwrap();
        assert_eq!(t.top(), 4, "under the shell's output");
        assert!(t.out.is_empty(), "nothing sent before the first frame");
        t.frame(&prompt(2), &prompt(3), (0, 0)).unwrap();
        assert_eq!(t.top(), 6, "right under what was printed");
        assert!(!moves_what_is_above(&String::from_utf8_lossy(&t.out)), "{:?}", String::from_utf8_lossy(&t.out));
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
    fn a_taller_screen_keeps_the_prompt_on_the_bottom_row() {
        // 100x30 to 100x40, the region on rows 27-29 and the caret on 28.
        let session = |lines: usize| {
            let mut t = Term::new(Vec::new(), Size { width: 100, height: 30 }, 0, 3).unwrap();
            t.frame(&prompt(lines), &prompt(3), (0, 1)).unwrap();
            t
        };
        let taller = Size { width: 100, height: 40 };
        // Partly in scrollback, the rows added under the region (Ghostty):
        // the region reaches down over them, the prompt at its bottom.
        let mut t = session(40);
        t.resize(taller, Some(28)).unwrap();
        assert_eq!((t.top(), t.height), (27, 13));
        t.frame(&[], &prompt(3), (0, 1)).unwrap();
        assert_eq!(t.caret_row, 11, "the spare rows above the prompt");
        t.frame(&prompt(4), &prompt(3), (0, 0)).unwrap();
        assert_eq!((t.top(), t.top() + t.height), (31, 40), "lines printed next fill them");
        // Scrollback pulled back down above it (tmux, kitty): already down.
        let mut t = session(40);
        t.resize(taller, Some(38)).unwrap();
        assert_eq!((t.top(), t.height), (37, 3));
        // The cursor not answering: its row is a guess, nothing is reserved.
        let mut t = session(40);
        t.resize(taller, None).unwrap();
        assert_eq!((t.top(), t.height), (27, 3));
        // All of the session on screen: nothing moves, the region reaches
        // down over the new rows.
        let mut t = session(4);
        assert_eq!((t.top(), t.height, t.caret_row), (4, 26, 24));
        t.resize(taller, Some(28)).unwrap();
        assert_eq!((t.top(), t.height), (4, 36));
        // Two resizes before a frame: measured from what the last frame sent.
        t.resize(Size { width: 100, height: 50 }, Some(28)).unwrap();
        assert_eq!((t.top(), t.height), (4, 46));
    }

    #[test]
    fn a_region_as_tall_as_the_screen_is_cleared_from_the_top_row_on_a_taller_one() {
        let mut t = Term::new(Vec::new(), Size { width: 100, height: 10 }, 0, 3).unwrap();
        t.frame(&[], &prompt(10), (0, 9)).unwrap();
        assert_eq!(t.top(), 0);
        let start = t.out.len();
        t.resize(Size { width: 100, height: 20 }, Some(9)).unwrap();
        t.flush().unwrap();
        let out = String::from_utf8_lossy(&t.out[start..]).into_owned();
        assert!(out.contains("\r\x1b[9A\x1b[1C\x1b[J"), "the old region cleared from the top row: {out:?}");
        assert_eq!((t.top(), t.height), (0, 20));
    }

    #[test]
    fn a_job_stop_after_a_taller_screen_takes_only_the_rows_to_the_bottom() {
        let mut t = Term::new(Vec::new(), Size { width: 100, height: 30 }, 0, 3).unwrap();
        t.frame(&prompt(40), &prompt(3), (0, 1)).unwrap();
        t.resize(Size { width: 100, height: 40 }, Some(28)).unwrap();
        t.frame(&[], &prompt(3), (0, 1)).unwrap();
        t.resume(Size { width: 100, height: 40 }, Some(30)).unwrap();
        assert_eq!((t.top(), t.height), (30, 10), "under the shell's output, nothing scrolled away");
        let mut t = Term::new(Vec::new(), Size { width: 100, height: 30 }, 0, 3).unwrap();
        t.frame(&prompt(40), &prompt(3), (0, 1)).unwrap();
        t.resume(Size { width: 100, height: 30 }, Some(29)).unwrap();
        assert_eq!((t.top(), t.height), (27, 3), "on the last row, the rows the prompt needs and no more scrolled up");
    }

    fn prompt(n: usize) -> Vec<Line<'static>> {
        (0..n).map(|i| Line::from(format!("row {i}"))).collect()
    }

    fn moves_what_is_above(out: &str) -> bool {
        out.split("\x1b[").skip(1).any(|seq| seq.find(|c: char| c.is_ascii_alphabetic()).is_some_and(|end| matches!(&seq[end..=end], "r" | "T" | "L")))
    }

    #[test]
    fn a_region_that_closes_under_scrollback_leaves_no_gap_in_it() {
        // Twenty lines on a ten-row screen: the session is in scrollback.
        let mut t = Term::new(Vec::new(), Size { width: 40, height: 10 }, 0, 3).unwrap();
        t.frame(&prompt(20), &prompt(3), (0, 0)).unwrap();
        // A menu opens and closes: nothing above is moved down to meet the
        // prompt, which would leave blank rows at the top for the next line
        // to push into scrollback between two lines of the conversation.
        t.frame(&[], &prompt(8), (0, 0)).unwrap();
        let start = t.out.len();
        t.frame(&[], &prompt(3), (0, 2)).unwrap();
        let out = String::from_utf8_lossy(&t.out[start..]).into_owned();
        assert!(!moves_what_is_above(&out), "{out:?}");
        assert_eq!((t.top(), t.height), (2, 8), "the region keeps its rows, down to the bottom");
        assert_eq!(t.caret_row, 7, "the prompt drawn at its bottom");
        // Lines printed next fill the rows it kept before scrolling.
        t.frame(&prompt(2), &prompt(3), (0, 0)).unwrap();
        assert_eq!((t.top(), t.height), (4, 6));
    }

    #[test]
    fn lines_printed_as_the_region_shrinks_leave_no_blank_rows_under_it() {
        let mut t = Term::new(Vec::new(), Size { width: 40, height: 10 }, 0, 3).unwrap();
        t.frame(&prompt(20), &prompt(8), (0, 0)).unwrap();
        // A slash command run from its menu: the menu closes as its line
        // is printed. The region reaches the bottom row.
        t.frame(&prompt(1), &prompt(3), (0, 0)).unwrap();
        assert_eq!(t.top() + t.height, 10);
    }

    #[test]
    fn a_region_reaches_the_bottom_and_text_fills_it_from_the_top() {
        // Ghostty forgets which rows of what insert and delete line move
        // were wrapped by the terminal, so a copy breaks at each: nothing
        // printed is ever moved but by scrolling.
        let mut t = Term::new(Vec::new(), Size { width: 40, height: 20 }, 0, 3).unwrap();
        t.frame(&prompt(4), &prompt(3), (0, 0)).unwrap();
        assert_eq!((t.top(), t.height), (4, 16), "from under the text to the bottom");
        assert_eq!(t.caret_row, 13, "the three rows at its bottom");
        let start = t.out.len();
        t.frame(&[], &prompt(8), (0, 0)).unwrap();
        t.frame(&[], &prompt(3), (0, 0)).unwrap();
        assert_eq!((t.top(), t.height), (4, 16), "taller and back within the rows it has");
        t.frame(&prompt(1), &prompt(3), (0, 0)).unwrap();
        assert_eq!((t.top(), t.height), (5, 15), "a line printed takes the row under the last");
        t.frame(&[], &prompt(18), (0, 0)).unwrap();
        assert_eq!((t.top(), t.height), (2, 18), "past the bottom, the screen scrolls");
        let out = String::from_utf8_lossy(&t.out[start..]).into_owned();
        assert!(!moves_what_is_above(&out) && !out.contains('M'), "{out:?}");
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
    fn a_link_reaches_scrollback_as_a_hyperlink_to_its_url() {
        let mut t = Term::new(Vec::new(), Size { width: 60, height: 10 }, 0, 2).unwrap();
        let mut fence = crate::look::Markdown::default();
        t.frame(&[crate::look::markdown_line("see [docs](https://krowk.com/d)", &mut fence)], &[Line::from("❯ "), Line::default()], (2, 0)).unwrap();
        let out = String::from_utf8_lossy(&t.into_inner()).into_owned();
        assert!(out.contains("\x1b]8;;https://krowk.com/d\x1b\\\x1b[4;36mdocs\x1b[0m\x1b]8;;\x1b\\"), "{out:?}");
        assert!(out.contains("\x1b]8;;https://krowk.com/d\x1b\\\x1b[36m\u{a0}↗\x1b[0m\x1b]8;;\x1b\\"), "{out:?}");
        assert!(!out.chars().any(|c| ('\u{E0000}'..='\u{E007F}').contains(&c)), "the URL's carrier never reaches the terminal");
    }

    #[test]
    fn a_link_in_the_live_region_is_a_hyperlink_cell_by_cell() {
        let mut t = Term::new(Vec::new(), Size { width: 60, height: 10 }, 0, 1).unwrap();
        let url = "https://github.com/krowkcom/krowk-cli/pull/133";
        let row = Line::from("#1".chars().map(|c| crate::look::linked(c.to_string(), ratatui::style::Style::new(), url)).collect::<Vec<_>>());
        t.frame(&[], &[row], (0, 0)).unwrap();
        let out = String::from_utf8_lossy(&t.into_inner()).into_owned();
        assert!(out.contains(&format!("\x1b]8;;{url}\x1b\\#\x1b]8;;\x1b\\\x1b]8;;{url}\x1b\\1\x1b]8;;\x1b\\")), "{out:?}");
        assert!(!out.chars().any(|c| ('\u{E0000}'..='\u{E007F}').contains(&c)), "the URL's carrier never reaches the terminal");
    }

    /// A terminal enough for what an inline frame sends: text, CR, LF that
    /// scrolls the top row into history at the bottom, relative moves,
    /// clears, autowrap; everything else read and dropped. A resize does as
    /// tmux's: a shorter screen drops the rows below the cursor first, then
    /// pushes its top rows into history; a taller one adds rows at the
    /// bottom; what is wider is cut.
    struct Vt {
        w: usize,
        h: usize,
        grid: Vec<Vec<char>>,
        history: Vec<String>,
        x: usize,
        y: usize,
        wrap: bool,
        /// Every private mode set, for what must never be.
        modes: Vec<String>,
    }

    impl Vt {
        fn new(w: u16, h: u16) -> Vt {
            let (w, h) = (usize::from(w), usize::from(h));
            Vt { w, h, grid: vec![vec![' '; w]; h], history: Vec::new(), x: 0, y: 0, wrap: true, modes: Vec::new() }
        }

        fn row(r: &[char]) -> String {
            r.iter().collect::<String>().trim_end().to_string()
        }

        fn lf(&mut self) {
            if self.y + 1 < self.h {
                self.y += 1;
            } else {
                let top = self.grid.remove(0);
                self.history.push(Vt::row(&top));
                self.grid.push(vec![' '; self.w]);
            }
        }

        fn feed(&mut self, bytes: &[u8]) {
            let text = String::from_utf8_lossy(bytes).into_owned();
            let mut cs = text.chars().peekable();
            while let Some(c) = cs.next() {
                match c {
                    '\x1b' => match cs.next() {
                        Some('[') => {
                            let mut p = String::new();
                            let fin = loop {
                                match cs.next() {
                                    Some(c) if c.is_ascii_alphabetic() || c == '~' => break c,
                                    Some(c) => p.push(c),
                                    None => break ' ',
                                }
                            };
                            let n = p.trim_start_matches(['?', '>', '<']).split(';').next().and_then(|n| n.parse::<usize>().ok()).unwrap_or(1).max(1);
                            if p.starts_with('?') && fin == 'h' {
                                self.modes.push(p.clone());
                            }
                            match (fin, p.as_str()) {
                                ('h', "?7") => self.wrap = true,
                                ('l', "?7") => self.wrap = false,
                                ('A', _) => self.y = self.y.saturating_sub(n),
                                ('B', _) => self.y = (self.y + n).min(self.h - 1),
                                ('C', _) => self.x = (self.x + n).min(self.w - 1),
                                ('J', "") => {
                                    for x in self.x..self.w {
                                        self.grid[self.y][x] = ' ';
                                    }
                                    for r in self.grid.iter_mut().skip(self.y + 1) {
                                        r.fill(' ');
                                    }
                                }
                                ('K', "") => self.grid[self.y][self.x.min(self.w - 1)..].fill(' '),
                                ('K', "1") => self.grid[self.y][..=self.x.min(self.w - 1)].fill(' '),
                                ('H' | 'J' | 'f' | 'r' | 'L' | 'M' | 'S' | 'T', _) => panic!("not sent inline: ESC[{p}{fin}"),
                                _ => {}
                            }
                        }
                        Some(']') => {
                            while let Some(c) = cs.next() {
                                if c == '\x07' || (c == '\x1b' && cs.next_if_eq(&'\\').is_some()) {
                                    break;
                                }
                            }
                        }
                        _ => {}
                    },
                    '\r' => self.x = 0,
                    '\n' => self.lf(),
                    c => {
                        if self.x >= self.w {
                            if self.wrap {
                                self.x = 0;
                                self.lf();
                            } else {
                                self.x = self.w - 1;
                            }
                        }
                        self.grid[self.y][self.x] = c;
                        self.x += 1;
                    }
                }
            }
        }

        fn resize(&mut self, w: u16, h: u16) {
            let (w, h) = (usize::from(w), usize::from(h));
            for r in &mut self.grid {
                r.resize(w, ' ');
            }
            while self.grid.len() > h {
                if self.y + 1 < self.grid.len() {
                    self.grid.pop();
                } else {
                    let top = self.grid.remove(0);
                    self.history.push(Vt::row(&top));
                    self.y -= 1;
                }
            }
            while self.grid.len() < h {
                self.grid.push(vec![' '; w]);
            }
            (self.w, self.h, self.x) = (w, h, self.x.min(w - 1));
        }

        fn screen(&self) -> Vec<String> {
            self.grid.iter().map(|r| Vt::row(r)).collect()
        }

        /// History and screen, as a copy of the whole pane reads them.
        fn all(&self) -> Vec<String> {
            self.history.iter().cloned().chain(self.screen()).collect()
        }
    }

    /// What `t` sent since last asked, into `vt`.
    fn pump(t: &mut Term<Vec<u8>>, vt: &mut Vt) {
        vt.feed(&std::mem::take(&mut t.out));
    }

    fn view(n: u16) -> Vec<Line<'static>> {
        (0..n).map(|i| Line::from(format!("view {i}"))).collect()
    }

    /// The pane less its blank rows: what is on it, in order.
    fn text(rows: &[String]) -> Vec<String> {
        rows.iter().filter(|r| !r.is_empty()).cloned().collect()
    }

    #[test]
    fn r_sub_11_the_view_fills_the_screen_and_closing_leaves_scrollback_and_prompt_as_they_were() {
        let (w, h) = (40, 10);
        let mut t = Term::new(Vec::new(), Size { width: w, height: h }, 0, 3).unwrap();
        let mut vt = Vt::new(w, h);
        t.frame(&prompt(25).iter().map(|l| Line::from(format!("line {}", l.spans[0].content))).collect::<Vec<_>>(), &prompt(3), (2, 2)).unwrap();
        pump(&mut t, &mut vt);
        let (history, screen) = (vt.history.clone(), vt.screen());
        assert_eq!(screen[9], "row 2", "{screen:?}");
        // Open: every row is the view's, nothing scrolled into history.
        t.view(&[], &view(h)).unwrap();
        pump(&mut t, &mut vt);
        assert_eq!(vt.screen(), (0..h).map(|i| format!("view {i}")).collect::<Vec<_>>());
        assert_eq!(vt.history, history, "scrollback untouched");
        // Redrawn, and lines arriving meanwhile: held, history untouched.
        t.view(&[Line::from("arrived")], &view(h).into_iter().rev().collect::<Vec<_>>()).unwrap();
        pump(&mut t, &mut vt);
        assert_eq!((vt.screen()[0].as_str(), &vt.history), ("view 9", &history));
        // Closed: the screen as it was, the line that arrived printed after
        // the conversation, where the next line goes.
        t.frame(&[], &prompt(3), (2, 2)).unwrap();
        pump(&mut t, &mut vt);
        let mut want: Vec<String> = history.iter().chain(&screen).filter(|r| r.starts_with("line")).cloned().collect();
        want.push("arrived".into());
        want.extend(["row 0", "row 1", "row 2"].map(String::from));
        assert_eq!(text(&vt.all()), want, "{:?}", vt.all());
        assert_eq!(vt.screen()[9], "row 2", "the prompt where it was");
        assert!(vt.modes.iter().all(|m| !matches!(m.as_str(), "?1049" | "?1047" | "?47" | "?1000" | "?1002" | "?1003" | "?1006")), "no alternate screen, no mouse: {:?}", vt.modes);
    }

    #[test]
    fn r_sub_11_closing_with_nothing_arrived_puts_back_exactly_what_was_on_screen() {
        let (w, h) = (30, 8);
        let mut t = Term::new(Vec::new(), Size { width: w, height: h }, 0, 2).unwrap();
        let mut vt = Vt::new(w, h);
        t.frame(&prompt(20), &prompt(2), (0, 1)).unwrap();
        pump(&mut t, &mut vt);
        let before = (vt.history.clone(), vt.screen());
        t.view(&[], &view(h)).unwrap();
        t.frame(&[], &prompt(2), (0, 1)).unwrap();
        pump(&mut t, &mut vt);
        assert_eq!((vt.history.clone(), vt.screen()), before);
    }

    #[test]
    fn r_sub_11_rows_never_printed_here_move_into_scrollback_rather_than_being_lost() {
        // The shell's output above where krowk started: nothing here can
        // draw it again, so the view scrolls it up rather than over it.
        let (w, h) = (30, 12);
        let mut vt = Vt::new(w, h);
        vt.feed(b"$ shell 0\r\n$ shell 1\r\n$ shell 2\r\n");
        let mut t = Term::new(Vec::new(), Size { width: w, height: h }, 3, 2).unwrap();
        t.frame(&[Line::from("hello"), Line::from("x".repeat(70))], &prompt(2), (0, 1)).unwrap();
        pump(&mut t, &mut vt);
        let before = text(&vt.all());
        t.view(&[], &view(h)).unwrap();
        pump(&mut t, &mut vt);
        assert_eq!(vt.screen()[0], "view 0");
        t.frame(&[], &prompt(2), (0, 1)).unwrap();
        pump(&mut t, &mut vt);
        assert_eq!(text(&vt.all()), before, "{:?}", vt.all());
        assert_eq!(vt.screen()[11], "row 1");
    }

    #[test]
    fn r_sub_11_a_resize_while_open_redraws_it_and_closing_leaves_no_stray_rows() {
        let (w, h) = (40, 10);
        let mut t = Term::new(Vec::new(), Size { width: w, height: h }, 0, 3).unwrap();
        let mut vt = Vt::new(w, h);
        t.frame(&prompt(25), &prompt(3), (0, 2)).unwrap();
        t.view(&[], &view(h)).unwrap();
        pump(&mut t, &mut vt);
        for (w, h) in [(40, 14), (30, 6), (36, 9)] {
            vt.resize(w, h);
            t.resize(Size { width: w, height: h }, Some(vt.y as u16)).unwrap();
            t.view(&[], &view(h)).unwrap();
            pump(&mut t, &mut vt);
            assert_eq!(vt.screen(), (0..h).map(|i| format!("view {i}")).collect::<Vec<_>>(), "redrawn whole at {w}x{h}");
        }
        t.frame(&[Line::from("after")], &prompt(3), (0, 2)).unwrap();
        pump(&mut t, &mut vt);
        let all = vt.all();
        assert!(!all.iter().any(|r| r.starts_with("view")), "a stray row of the view: {all:?}");
        assert_eq!(vt.screen().last().map(String::as_str), Some("row 2"), "{all:?}");
        let rows: Vec<&String> = all.iter().filter(|r| r.starts_with("row ")).collect();
        assert_eq!(rows.len(), 25 + 3, "the conversation once, the prompt once: {all:?}");
        let after = all.iter().position(|r| r == "after").unwrap();
        assert_eq!(all[after - 1], "row 24", "printed after the conversation: {all:?}");
    }

    fn said(n: usize, what: &str) -> Vec<Line<'static>> {
        (0..n).map(|i| Line::from(format!("{what} {i}"))).collect()
    }

    #[test]
    fn r_sub_11_leaving_krowk_with_the_view_open_puts_the_conversation_back() {
        let (w, h) = (40, 10);
        let mut t = Term::new(Vec::new(), Size { width: w, height: h }, 0, 3).unwrap();
        let mut vt = Vt::new(w, h);
        t.frame(&said(20, "line"), &prompt(3), (0, 2)).unwrap();
        t.view(&[], &view(h)).unwrap();
        t.view(&[Line::from("arrived")], &view(h)).unwrap();
        t.finish().unwrap();
        pump(&mut t, &mut vt);
        let mut want = said(20, "line").iter().map(|l| l.spans[0].content.to_string()).collect::<Vec<_>>();
        want.push("arrived".into());
        assert_eq!(text(&vt.all()), want, "{:?}", vt.all());
        assert_eq!(vt.screen()[vt.y - 1], "arrived", "the shell's prompt goes right under it");
    }

    #[test]
    fn r_sub_11_a_job_stop_with_the_view_open_puts_the_conversation_back_and_reopens_it() {
        let (w, h) = (40, 10);
        let mut t = Term::new(Vec::new(), Size { width: w, height: h }, 0, 3).unwrap();
        let mut vt = Vt::new(w, h);
        t.frame(&said(20, "line"), &prompt(3), (0, 2)).unwrap();
        t.view(&[Line::from("arrived")], &view(h)).unwrap();
        t.finish().unwrap();
        pump(&mut t, &mut vt);
        vt.feed(b"[1]+ Stopped\r\n$ fg\r\n");
        t.resume(Size { width: w, height: h }, Some(vt.y as u16)).unwrap();
        t.view(&[], &view(h)).unwrap();
        pump(&mut t, &mut vt);
        assert_eq!(vt.screen(), (0..h).map(|i| format!("view {i}")).collect::<Vec<_>>());
        t.frame(&[], &prompt(3), (0, 2)).unwrap();
        pump(&mut t, &mut vt);
        let all = text(&vt.all());
        assert!(!all.iter().any(|r| r.starts_with("view")), "{all:?}");
        let mut want = said(20, "line").iter().map(|l| l.spans[0].content.to_string()).collect::<Vec<_>>();
        want.extend(["arrived", "[1]+ Stopped", "$ fg", "row 0", "row 1", "row 2"].map(String::from));
        assert_eq!(all, want);
    }

    #[test]
    fn r_sub_11_many_lines_arriving_while_it_is_open_are_all_printed_and_nothing_it_covered_is_lost() {
        let (w, h) = (40, 10);
        let mut t = Term::new(Vec::new(), Size { width: w, height: h }, 0, 3).unwrap();
        let mut vt = Vt::new(w, h);
        t.frame(&said(20, "line"), &prompt(3), (0, 2)).unwrap();
        t.view(&[], &view(h)).unwrap();
        t.view(&said(TAIL + 88, "held"), &view(h)).unwrap();
        t.frame(&[Line::from("last")], &prompt(3), (0, 2)).unwrap();
        pump(&mut t, &mut vt);
        let mut want: Vec<String> = said(20, "line").iter().chain(&said(TAIL + 88, "held")).map(|l| l.spans[0].content.to_string()).collect();
        want.extend(["last", "row 0", "row 1", "row 2"].map(String::from));
        assert_eq!(text(&vt.all()), want);
    }

    #[test]
    fn r_sub_11_a_terminal_that_does_not_reflow_is_put_back_at_the_rows_it_printed() {
        // xterm keeps a line printed at 40 columns in its two rows on a
        // wider screen: measured at 80, the view would take it for one.
        let (w, h) = (40, 10);
        let mut t = Term::new(Vec::new(), Size { width: w, height: h }, 0, 3).unwrap();
        t.reflows = false;
        let mut vt = Vt::new(w, h);
        let mut lines = said(3, "line");
        lines.insert(1, Line::from("x".repeat(60)));
        t.frame(&lines, &prompt(3), (0, 2)).unwrap();
        pump(&mut t, &mut vt);
        vt.resize(80, h);
        t.resize(Size { width: 80, height: h }, Some(vt.y as u16)).unwrap();
        t.frame(&[], &prompt(3), (0, 2)).unwrap();
        pump(&mut t, &mut vt);
        let before = text(&vt.all());
        t.view(&[], &view(h)).unwrap();
        t.frame(&[], &prompt(3), (0, 2)).unwrap();
        pump(&mut t, &mut vt);
        let after = text(&vt.all());
        // Printed back at 80 columns, the wide line is one row now.
        assert_eq!(after.iter().filter(|r| r.starts_with('x')).map(|r| r.len()).sum::<usize>(), 60, "the wide line once: {after:?}");
        assert_eq!(after.iter().filter(|r| r.starts_with("line")).count(), 3, "measured at 80, line 0 would be printed twice: {before:?} {after:?}");
    }

    #[test]
    fn r_sub_11_the_view_sends_no_alternate_screen_and_hides_the_cursor_only_while_open() {
        let mut t = Term::new(Vec::new(), Size { width: 40, height: 10 }, 0, 3).unwrap();
        t.frame(&prompt(4), &prompt(3), (0, 2)).unwrap();
        let start = t.out.len();
        t.view(&[], &view(10)).unwrap();
        let open = String::from_utf8_lossy(&t.out[start..]).into_owned();
        assert!(open.contains("\x1b[?25l"), "{open:?}");
        let start = t.out.len();
        t.frame(&[], &prompt(3), (0, 2)).unwrap();
        let closed = String::from_utf8_lossy(&t.out[start..]).into_owned();
        assert!(closed.contains("\x1b[?25h"), "{closed:?}");
        let out = String::from_utf8_lossy(&t.into_inner()).into_owned();
        for seq in ["\x1b[?1049", "\x1b[?1047", "\x1b[?47", "\x1b[?1000", "\x1b[?1002", "\x1b[?1003", "\x1b[?1006", "\x1b[2J", "\x1b[>"] {
            assert!(!out.contains(seq), "{seq:?} in {out:?}");
        }
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
