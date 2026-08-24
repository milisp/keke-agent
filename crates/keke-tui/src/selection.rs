//! Selecting text with the mouse while keke owns it.
//!
//! Capturing the mouse is what makes a click able to open a tool call, and it
//! is also what takes drag-select away from the terminal. Only one of those is
//! keke's to give back: so the drag is answered here, over what the frame
//! actually drew, and the release puts it on the clipboard.
//!
//! Columns are counted in `char`s of the drawn line. A double-width glyph is
//! therefore off by one for the rest of its row — visible in CJK text, and the
//! price of not carrying a width table for a highlight nobody measures.

use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::text::Span;

/// A point in the drawn body, as `(row, column)` in screen cells.
type Point = (u16, u16);

/// What the mouse is doing to the transcript right now.
#[derive(Debug, Default)]
pub(crate) struct Selection {
    /// Where the left button went down, until it comes back up.
    press: Option<Point>,
    /// Set once the pointer has left the cell it was pressed in: what tells a
    /// click apart from a drag, and so a toggle apart from a selection.
    dragged: bool,
    /// The selected span, anchor first. Outlives the drag so a reader can see
    /// what they copied.
    range: Option<(Point, Point)>,
    /// The plain text of each row of the body, as this frame drew it.
    rows: Vec<String>,
    /// Where the body starts, so a screen row can be found in `rows`.
    top: u16,
}

impl Selection {
    /// Told by `draw` what the body holds, so a drag has something to cut from.
    pub(crate) fn set_rows(&mut self, top: u16, rows: Vec<String>) {
        self.top = top;
        self.rows = rows;
    }

    pub(crate) fn press(&mut self, at: Point) {
        self.press = Some(at);
        self.dragged = false;
        self.range = None;
    }

    /// Extend the drag. Returns whether anything is selected yet.
    pub(crate) fn drag_to(&mut self, at: Point) -> bool {
        let Some(anchor) = self.press else {
            return false;
        };
        if at != anchor {
            self.dragged = true;
        }
        if self.dragged {
            self.range = Some((anchor, at));
        }
        self.dragged
    }

    /// Finish the gesture. `Some(text)` when it was a drag with something in
    /// it; `None` when it was a click, which belongs to whatever is under it.
    pub(crate) fn release(&mut self) -> Option<String> {
        let dragged = self.dragged;
        self.press = None;
        self.dragged = false;
        if !dragged {
            self.range = None;
            return None;
        }
        let text = self.text();
        if text.is_empty() {
            self.range = None;
            return None;
        }
        Some(text)
    }

    pub(crate) fn clear(&mut self) {
        *self = Self {
            rows: std::mem::take(&mut self.rows),
            top: self.top,
            ..Self::default()
        };
    }

    /// The selection in reading order, as `(row, from, to)` per row, with `to`
    /// exclusive.
    fn spans(&self) -> Vec<(u16, usize, usize)> {
        let Some((anchor, cursor)) = self.range else {
            return Vec::new();
        };
        let (start, end) = if anchor <= cursor {
            (anchor, cursor)
        } else {
            (cursor, anchor)
        };
        (start.0..=end.0)
            .filter_map(|row| {
                let text = self.rows.get(usize::from(row.checked_sub(self.top)?))?;
                let width = text.chars().count();
                let from = if row == start.0 {
                    usize::from(start.1).min(width)
                } else {
                    0
                };
                // The release column is inclusive: a drag that stops on a
                // character has selected it, which is what the highlight under
                // the pointer says.
                let to = if row == end.0 {
                    (usize::from(end.1) + 1).min(width)
                } else {
                    width
                };
                (from < to).then_some((row, from, to))
            })
            .collect()
    }

    fn text(&self) -> String {
        let rows: Vec<String> = self
            .spans()
            .into_iter()
            .map(|(row, from, to)| {
                let text = &self.rows[usize::from(row - self.top)];
                let cut: String = text.chars().take(to).skip(from).collect();
                cut.trim_end().to_string()
            })
            .collect();
        rows.join("\n").trim_end().to_string()
    }

    /// Restyle `line`, drawn at `row`, to show what is selected in it.
    pub(crate) fn highlight(&self, row: u16, line: Line<'static>) -> Line<'static> {
        let Some(&(_, from, to)) = self.spans().iter().find(|(at, _, _)| *at == row) else {
            return line;
        };
        let mut column = 0usize;
        let spans = line
            .spans
            .into_iter()
            .flat_map(|span| split(span, &mut column, from, to))
            .collect::<Vec<_>>();
        Line::from(spans).style(line.style)
    }
}

/// Cut one span where the selection starts and ends inside it.
///
/// `column` walks the row so each span knows where it sits; the reversed piece
/// keeps the span's own colours, because a selection marks what is under it
/// rather than replacing it.
fn split(span: Span<'static>, column: &mut usize, from: usize, to: usize) -> Vec<Span<'static>> {
    let start = *column;
    let chars: Vec<char> = span.content.chars().collect();
    *column += chars.len();
    let end = *column;
    if end <= from || start >= to {
        return vec![span];
    }
    let cut = |range: std::ops::Range<usize>| -> String { chars[range].iter().collect() };
    let mid_from = from.saturating_sub(start);
    let mid_to = (to - start).min(chars.len());
    let mut pieces = Vec::with_capacity(3);
    if mid_from > 0 {
        pieces.push(Span::styled(cut(0..mid_from), span.style));
    }
    pieces.push(Span::styled(
        cut(mid_from..mid_to),
        span.style.add_modifier(Modifier::REVERSED),
    ));
    if mid_to < chars.len() {
        pieces.push(Span::styled(cut(mid_to..chars.len()), span.style));
    }
    pieces
}
