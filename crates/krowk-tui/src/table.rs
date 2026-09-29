//! Markdown tables in an answer. A table's rows are held while it streams
//! (`App::push_md`) and drawn once it ends, as a grid fitted to the width:
//! each cell in the answer's light markdown, wrapped inside its column. A
//! table that cannot be drawn so is shown as the lines it was typed as.
//!
//! Only rows that start with a pipe are held; a row without one reads as
//! text until the line after it, which would hold every line with a `|`.

use crate::app::wrap_line;
use crate::look;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// The narrowest a column is drawn.
const MIN_COLUMN: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Align {
    Left,
    Center,
    Right,
}

/// Whether `line` may be a table's row: it starts with a pipe.
pub fn is_row(line: &str) -> bool {
    line.trim_start().starts_with('|')
}

/// `rows` as a grid at most `width` columns wide: a header, a delimiter
/// row, then the body. `None` when they are no table or it cannot fit.
pub fn render(rows: &[String], width: usize) -> Option<Vec<Line<'static>>> {
    let [head, delimiter, body @ ..] = rows else { return None };
    let aligns = alignments(delimiter)?;
    let n = aligns.len();
    if cells(head).len() != n {
        return None;
    }
    let grid: Vec<Vec<Line<'static>>> = std::iter::once(head).chain(body).enumerate().map(|(r, row)| styled(row, n, r == 0)).collect();
    let widths = fit(&grid, width.checked_sub(3 * n + 1)?)?;
    let rule = |l: &str, m: &str, r: &str| Line::from(Span::styled(format!("{l}{}{r}", widths.iter().map(|w| "─".repeat(w + 2)).collect::<Vec<_>>().join(m)), look::border()));
    let mut out = vec![rule("┌", "┬", "┐")];
    for (r, row) in grid.into_iter().enumerate() {
        if r == 1 {
            out.push(rule("├", "┼", "┤"));
        }
        out.extend(drawn(row, &widths, &aligns));
    }
    out.push(rule("└", "┴", "┘"));
    Some(out)
}

/// A row's cells, the pipes around it taken off; `\|` is a pipe in a cell.
fn cells(row: &str) -> Vec<String> {
    let row = row.trim();
    let row = row.strip_prefix('|').unwrap_or(row);
    let row = if row.ends_with('|') && !row.ends_with("\\|") { &row[..row.len() - 1] } else { row };
    let mut out = vec![String::new()];
    let mut chars = row.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => out.last_mut().unwrap().push(chars.next().unwrap()),
            '|' => out.push(String::new()),
            c => out.last_mut().unwrap().push(c),
        }
    }
    out.into_iter().map(|c| c.trim().to_string()).collect()
}

/// Each column's alignment, when `row` is a delimiter row: `---`, `:--`,
/// `:-:` or `--:` in every cell.
fn alignments(row: &str) -> Option<Vec<Align>> {
    cells(row)
        .iter()
        .map(|c| {
            let dashes = c.trim_start_matches(':').trim_end_matches(':');
            (!dashes.is_empty() && dashes.chars().all(|d| d == '-')).then(|| match (c.starts_with(':'), c.ends_with(':')) {
                (true, true) => Align::Center,
                (false, true) => Align::Right,
                _ => Align::Left,
            })
        })
        .collect()
}

/// A row's `n` cells in light markdown, the header's bold; missing cells
/// are empty and extra ones dropped.
fn styled(row: &str, n: usize, header: bool) -> Vec<Line<'static>> {
    let mut cells = cells(row);
    cells.resize(n, String::new());
    cells.iter().map(|c| Line::from(if header { look::emphasised(look::inline(c), look::bold()) } else { look::inline(c) })).collect()
}

/// Each column's width, their sum at most `room`: the widest column gives
/// a column at a time until they fit. A column is no narrower than
/// `MIN_COLUMN`, nor than a URL it shows whole; `None` when that is too
/// wide.
fn fit(grid: &[Vec<Line<'static>>], room: usize) -> Option<Vec<usize>> {
    let column = |c: usize| grid.iter().map(move |row| &row[c]);
    let n = grid[0].len();
    let mut widths: Vec<usize> = (0..n).map(|c| column(c).map(Line::width).max().unwrap_or(0).max(1)).collect();
    let floors: Vec<usize> = (0..n).map(|c| column(c).map(unbroken).max().unwrap_or(0).max(MIN_COLUMN)).collect();
    while widths.iter().sum::<usize>() > room {
        let c = (0..n).filter(|&c| widths[c] > floors[c]).max_by_key(|&c| widths[c])?;
        widths[c] -= 1;
    }
    Some(widths)
}

/// The widest URL `line` shows as itself, which `wrap_line` never breaks.
fn unbroken(line: &Line<'static>) -> usize {
    line.spans
        .iter()
        .filter_map(|s| look::link_target(s).filter(|(t, u)| look::shows_its_url(t, u)).map(|(t, _)| t.width()))
        .max()
        .unwrap_or(0)
}

/// One row of the grid: each cell wrapped to its column, the row as tall
/// as its tallest cell.
fn drawn(row: Vec<Line<'static>>, widths: &[usize], aligns: &[Align]) -> Vec<Line<'static>> {
    let wrapped: Vec<Vec<Line<'static>>> = row.into_iter().zip(widths).map(|(cell, &w)| wrap_line(cell, w)).collect();
    let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
    (0..height)
        .map(|k| {
            let mut spans = vec![Span::styled("│", look::border())];
            for ((cell, &w), &align) in wrapped.iter().zip(widths).zip(aligns) {
                spans.extend(padded(cell.get(k), w, align));
                spans.push(Span::styled("│", look::border()));
            }
            Line::from(spans)
        })
        .collect()
}

/// A cell's line padded to `w` columns as its column is aligned, with a
/// space either side.
fn padded(line: Option<&Line<'static>>, w: usize, align: Align) -> Vec<Span<'static>> {
    let spans = line.map(|l| l.spans.clone()).unwrap_or_default();
    let gap = w.saturating_sub(spans.iter().map(Span::width).sum());
    let left = match align {
        Align::Left => 0,
        Align::Center => gap / 2,
        Align::Right => gap,
    };
    let space = |n: usize| Span::styled(" ".repeat(n), Style::new());
    let mut out = vec![space(1 + left)];
    out.extend(spans);
    out.push(space(1 + gap - left));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.spans.iter().map(|s| look::link_target(s).map_or(s.content.to_string(), |(t, _)| t.to_string())).collect()).collect()
    }

    fn rows(s: &str) -> Vec<String> {
        s.lines().map(String::from).collect()
    }

    #[test]
    fn a_table_is_drawn_as_a_grid() {
        let t = render(&rows("| Name | Size |\n|:-----|-----:|\n| `a.rs` | 12 |\n| **b** | 3 |"), 80).unwrap();
        assert_eq!(
            text(&t),
            ["┌──────┬──────┐", "│ Name │ Size │", "├──────┼──────┤", "│ a.rs │   12 │", "│ b    │    3 │", "└──────┴──────┘"]
        );
        assert!(t[1].spans.iter().any(|s| s.content == "Name" && s.style == look::bold()), "the header is bold");
        assert!(t[3].spans.iter().any(|s| s.content == "a.rs" && s.style == look::code()), "a cell is light markdown");
    }

    #[test]
    fn a_wide_table_wraps_inside_its_columns() {
        let t = render(&rows("| a | b |\n|---|:-:|\n| one two three four | x |"), 16).unwrap();
        assert_eq!(text(&t), ["┌──────────┬───┐", "│ a        │ b │", "├──────────┼───┤", "│ one two  │ x │", "│ three    │   │", "│ four     │   │", "└──────────┴───┘"]);
        assert!(t.iter().all(|l| l.width() <= 16));
    }

    #[test]
    fn cells_are_split_on_unescaped_pipes() {
        assert_eq!(cells("| a \\| b | c |"), ["a | b", "c"]);
        assert_eq!(cells("|a|b"), ["a", "b"]);
        assert_eq!(cells("| a | b \\|"), ["a", "b |"]);
    }

    #[test]
    fn a_ragged_row_is_filled_or_cut_to_the_header() {
        let t = render(&rows("| a | b |\n|---|---|\n| 1 |\n| 1 | 2 | 3 |"), 80).unwrap();
        assert_eq!(text(&t)[3..5], ["│ 1 │   │", "│ 1 │ 2 │"]);
    }

    #[test]
    fn what_is_no_table_or_cannot_fit_is_not_drawn() {
        assert!(render(&rows("| a | b |"), 80).is_none(), "no delimiter row");
        assert!(render(&rows("| a | b |\n| c | d |"), 80).is_none(), "no delimiter row");
        assert!(render(&rows("| a | b |\n|---|"), 80).is_none(), "a column short");
        assert!(render(&rows("| a | b |\n|---|---|"), 8).is_none(), "too narrow");
        let url = "| see |\n|---|\n| https://krowk.com/a/rather/long/path |";
        assert!(render(&rows(url), 20).is_none(), "a URL is never broken");
        assert!(render(&rows(url), 60).is_some());
    }
}
