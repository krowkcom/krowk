//! Markdown tables in an answer. A table's rows are held while it streams
//! (`App::push_md`) and drawn once it ends, each cell in the answer's light
//! markdown. Where there is room, as columns: the header bold over a thin
//! rule, each cell wrapped inside its column. Where there is not, each row
//! as a record, a line for each column: `Header  value`. A table too narrow
//! even for that is shown as the lines it was typed as.
//!
//! Only rows that start with a pipe are held; a row without one reads as
//! text until the line after it, which would hold every line with a `|`.

use crate::app::wrap_line;
use crate::look;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// The narrowest a column is squeezed to; narrower, the rows are records.
const MIN_COLUMN: usize = 8;
/// Between two columns.
const GAP: usize = 2;

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

/// `rows` at most `width` columns wide: a header, a delimiter row, then the
/// body. `None` when they are no table or it cannot fit.
pub fn render(rows: &[String], width: usize) -> Option<Vec<Line<'static>>> {
    let [head, delimiter, body @ ..] = rows else { return None };
    let aligns = alignments(delimiter)?;
    let n = aligns.len();
    if cells(head).len() != n {
        return None;
    }
    let head = styled(head, n, true);
    let body: Vec<Vec<Line<'static>>> = body.iter().map(|row| styled(row, n, false)).collect();
    columns(&head, &body, &aligns, width).or_else(|| records(&head, &body, width))
}

/// The table as columns: the header over a rule, then the body; a blank
/// line between rows when any of them wraps.
fn columns(head: &[Line<'static>], body: &[Vec<Line<'static>>], aligns: &[Align], width: usize) -> Option<Vec<Line<'static>>> {
    let widths = fit(head, body, width.checked_sub(GAP * (head.len() - 1))?)?;
    let rule = widths.iter().map(|&w| "─".repeat(w)).collect::<Vec<_>>().join(&" ".repeat(GAP));
    let mut out = drawn(head.to_vec(), &widths, aligns);
    out.push(Line::from(Span::styled(rule, look::border())));
    let rows: Vec<Vec<Line<'static>>> = body.iter().map(|row| drawn(row.clone(), &widths, aligns)).collect();
    let spaced = rows.iter().any(|r| r.len() > 1);
    for (i, row) in rows.into_iter().enumerate() {
        if spaced && i > 0 {
            out.push(Line::default());
        }
        out.extend(row);
    }
    Some(out)
}

/// The table as records, a blank line apart: each row a line for each
/// column, its header before its value.
fn records(head: &[Line<'static>], body: &[Vec<Line<'static>>], width: usize) -> Option<Vec<Line<'static>>> {
    let label = head.iter().map(Line::width).max().unwrap_or(0).min(width / 3);
    let value = width.checked_sub(label + GAP).filter(|&v| v >= MIN_COLUMN)?;
    let mut out = Vec::new();
    for (i, row) in body.iter().enumerate() {
        if i > 0 {
            out.push(Line::default());
        }
        for (h, cell) in head.iter().zip(row) {
            out.extend(drawn(vec![h.clone(), cell.clone()], &[label, value], &[Align::Left, Align::Left]));
        }
    }
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
/// a column at a time until they fit. A column is squeezed no narrower
/// than `MIN_COLUMN`, nor than a URL it shows whole; `None` when that is
/// still too wide.
fn fit(head: &[Line<'static>], body: &[Vec<Line<'static>>], room: usize) -> Option<Vec<usize>> {
    let column = |c: usize| std::iter::once(&head[c]).chain(body.iter().map(move |row| &row[c]));
    let n = head.len();
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

/// One row: each cell wrapped to its column, the row as tall as its
/// tallest cell, the columns `GAP` apart.
fn drawn(row: Vec<Line<'static>>, widths: &[usize], aligns: &[Align]) -> Vec<Line<'static>> {
    let wrapped: Vec<Vec<Line<'static>>> = row.into_iter().zip(widths).map(|(cell, &w)| wrap_line(cell, w).into_iter().enumerate().map(unindented).collect()).collect();
    let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
    (0..height)
        .map(|k| {
            let mut spans = Vec::new();
            for (c, ((cell, &w), &align)) in wrapped.iter().zip(widths).zip(aligns).enumerate() {
                if c > 0 {
                    spans.push(space(GAP));
                }
                spans.extend(padded(cell.get(k), w, align));
            }
            let mut line = Line::from(spans);
            // No trailing blanks: a row is as wide as what it shows.
            while line.spans.last().is_some_and(|s| s.content.trim().is_empty()) {
                line.spans.pop();
            }
            line
        })
        .collect()
}

/// A cell's `k`th row without the space a word broken at the column's
/// edge leaves before the next.
fn unindented((k, mut line): (usize, Line<'static>)) -> Line<'static> {
    if k > 0
        && let Some(first) = line.spans.first_mut()
    {
        first.content = first.content.trim_start_matches(' ').to_string().into();
    }
    line
}

/// A cell's line padded to `w` columns as its column is aligned.
fn padded(line: Option<&Line<'static>>, w: usize, align: Align) -> Vec<Span<'static>> {
    let spans = line.map(|l| l.spans.clone()).unwrap_or_default();
    let gap = w.saturating_sub(spans.iter().map(Span::width).sum());
    let left = match align {
        Align::Left => 0,
        Align::Center => gap / 2,
        Align::Right => gap,
    };
    let mut out = vec![space(left)];
    out.extend(spans);
    out.push(space(gap - left));
    out
}

fn space(n: usize) -> Span<'static> {
    Span::styled(" ".repeat(n), Style::new())
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
    fn a_table_is_drawn_as_columns() {
        let t = render(&rows("| Name | Size |\n|:-----|-----:|\n| `a.rs` | 12 |\n| **b** | 3 |"), 80).unwrap();
        assert_eq!(text(&t), ["Name  Size", "────  ────", "a.rs    12", "b        3"]);
        assert!(t[0].spans.iter().any(|s| s.content == "Name" && s.style == look::bold()), "the header is bold");
        assert!(t[2].spans.iter().any(|s| s.content == "a.rs" && s.style == look::code()), "a cell is light markdown");
    }

    #[test]
    fn a_wide_table_wraps_inside_its_columns_and_spaces_its_rows() {
        let t = render(&rows("| a | b |\n|---|:-:|\n| one two three four five | x |\n| six | yyy |"), 16).unwrap();
        assert_eq!(text(&t), ["a             b", "───────────  ───", "one two       x", "three four", "five", "", "six          yyy"]);
        assert!(t.iter().all(|l| l.width() <= 16));
    }

    #[test]
    fn a_table_too_wide_for_its_columns_is_drawn_as_records() {
        let t = render(&rows("| Name | Kind | What it does |\n|---|---|---|\n| push | command | publishes a file |\n| pull | command | fetches one |"), 20).unwrap();
        assert_eq!(text(&t), ["Name    push", "Kind    command", "What    publishes a", "it      file", "does", "", "Name    pull", "Kind    command", "What    fetches one", "it", "does"]);
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
        assert_eq!(text(&t)[2..], ["1", "1  2"]);
    }

    #[test]
    fn what_is_no_table_or_cannot_fit_is_not_drawn() {
        assert!(render(&rows("| a | b |"), 80).is_none(), "no delimiter row");
        assert!(render(&rows("| a | b |\n| c | d |"), 80).is_none(), "no delimiter row");
        assert!(render(&rows("| a | b |\n|---|"), 80).is_none(), "a column short");
        assert!(render(&rows("| name | value |\n|---|---|\n| something long | other long |"), 12).is_none(), "too narrow even for records");
        let url = "| see | where |\n|---|---|\n| docs | https://krowk.com/a/rather/long/path |";
        assert_eq!(text(&render(&rows(url), 30).unwrap())[1], "where  https://krowk.com/a/rather/long/path\u{a0}↗", "a URL is never broken");
    }
}
