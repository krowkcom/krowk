//! The terminal the TUI draws on, as `tui.screen` chose it: fullscreen
//! (`full`, the default) or inline (`term`). The loop drives either the same
//! way; scrolling is fullscreen's alone, inline leaving it to the terminal.

use crate::full::Full;
use crate::term::Term;
use ratatui::layout::Size;
use ratatui::text::Line;
use std::io::{self, Write};

pub enum Screen<W: Write> {
    Full(Full<W>),
    Inline(Term<W>),
}

impl<W: Write> Screen<W> {
    pub fn size(&self) -> Size {
        match self {
            Screen::Full(t) => t.size(),
            Screen::Inline(t) => t.size(),
        }
    }

    /// Whether a resize or a job stop needs to know where the cursor is.
    pub fn inline(&self) -> bool {
        matches!(self, Screen::Inline(_))
    }

    pub fn frame(&mut self, lines: &[Line<'static>], rows: &[Line<'static>], caret: (u16, u16)) -> io::Result<()> {
        match self {
            Screen::Full(t) => t.frame(lines, rows, caret),
            Screen::Inline(t) => t.frame(lines, rows, caret),
        }
    }

    pub fn title(&mut self, title: &str) -> io::Result<()> {
        match self {
            Screen::Full(t) => t.title(title),
            Screen::Inline(t) => t.title(title),
        }
    }

    pub fn clipboard(&mut self, text: &str) -> io::Result<()> {
        match self {
            Screen::Full(t) => t.clipboard(text),
            Screen::Inline(t) => t.clipboard(text),
        }
    }

    pub fn steady(&mut self, on: bool) -> io::Result<()> {
        match self {
            Screen::Full(t) => t.steady(on),
            Screen::Inline(t) => t.steady(on),
        }
    }

    pub fn wipe(&mut self) -> io::Result<()> {
        match self {
            Screen::Full(t) => t.wipe(),
            Screen::Inline(t) => t.wipe(),
        }
    }

    /// The terminal changed size; `cursor_row` asks where the cursor is,
    /// which only inline needs.
    pub fn resize(&mut self, size: Size, cursor_row: impl FnOnce() -> Option<u16>) -> io::Result<()> {
        match self {
            Screen::Full(t) => t.resize(size),
            Screen::Inline(t) => t.resize(size, cursor_row()),
        }
    }

    /// Back after the terminal was given up (`finish`).
    pub fn resume(&mut self, size: Size, cursor_row: impl FnOnce() -> Option<u16>) -> io::Result<()> {
        match self {
            Screen::Full(t) => t.resume(size),
            Screen::Inline(t) => t.resume(size, cursor_row()),
        }
    }

    /// The terminal handed back as the shell had it.
    pub fn finish(&mut self) -> io::Result<()> {
        match self {
            Screen::Full(t) => t.finish(),
            Screen::Inline(t) => t.finish(),
        }
    }

    /// Leaving: fullscreen prints the conversation onto the shell's screen;
    /// inline's is in scrollback already.
    pub fn leave(&mut self) -> io::Result<()> {
        self.finish()?;
        match self {
            Screen::Full(t) => t.print_conversation(),
            Screen::Inline(_) => Ok(()),
        }
    }

    /// Scrolls the conversation `by` rows, up when positive; false when
    /// the terminal scrolls it instead (inline).
    pub fn scroll(&mut self, by: isize) -> bool {
        match self {
            Screen::Full(t) => {
                t.scroll(by);
                true
            }
            Screen::Inline(_) => false,
        }
    }

    /// A page of the conversation, in rows.
    pub fn page(&self) -> isize {
        match self {
            Screen::Full(t) => t.page(),
            Screen::Inline(_) => 0,
        }
    }

    /// The left button pressed, dragged and let go over the conversation
    /// (fullscreen's selection); true when the screen changed. `release`
    /// gives what was selected, to copy.
    pub fn press(&mut self, x: u16, y: u16) -> bool {
        match self {
            Screen::Full(t) => t.press(x, y),
            Screen::Inline(_) => false,
        }
    }

    pub fn drag(&mut self, x: u16, y: u16) -> bool {
        match self {
            Screen::Full(t) => t.drag(x, y),
            Screen::Inline(_) => false,
        }
    }

    pub fn release(&mut self, x: u16, y: u16) -> Option<String> {
        match self {
            Screen::Full(t) => t.release(x, y),
            Screen::Inline(_) => None,
        }
    }

    /// Back to the bottom of the conversation.
    pub fn follow(&mut self) {
        if let Screen::Full(t) = self {
            t.follow();
        }
    }

    pub fn into_inner(self) -> W {
        match self {
            Screen::Full(t) => t.into_inner(),
            Screen::Inline(t) => t.into_inner(),
        }
    }
}
