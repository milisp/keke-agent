//! Selecting text with the mouse while keke owns it.
//!
//! Capturing the mouse is what makes a click able to open a tool call, and it
//! is also what takes drag-select away from the terminal. Only one of those is
//! keke's to give back: so the drag is answered here, over what the frame
//! actually drew, and the release puts it on the clipboard.
//!
//! Rows are keyed by absolute screen row rather than an offset into a single
//! block, because more than one widget hands its text here — the transcript
//! body and the composer are drawn in separate, non-adjacent areas of the
//! frame, and a sparse map is what lets both contribute without one
//! overwriting the other or the gap between them meaning anything.
//!
//! A mouse column arrives in terminal cells, but a double-width glyph — CJK
//! text, most visibly — is one `char` and two cells. Every column is walked
//! through [`char_at_cell`] before it is used as a `char` index, so a
//! selection that starts or ends past a wide glyph lands on the right
//! character instead of drifting by one for the rest of the row.

use std::collections::BTreeMap;

use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthChar as _;

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
    /// The plain text of each drawn row, keyed by its absolute screen row.
    rows: BTreeMap<u16, String>,
}

impl Selection {
    /// Told by `draw` what the transcript body holds, so a drag has something
    /// to cut from. Replaces every row handed in by a previous frame,
    /// including ones contributed by [`Self::add_rows`].
    pub(crate) fn set_rows(&mut self, top: u16, rows: Vec<String>) {
        self.rows.clear();
        self.add_rows(top, rows);
    }

    /// Told by `draw` what another area — the composer, say — holds this
    /// frame, alongside whatever [`Self::set_rows`] already contributed.
    pub(crate) fn add_rows(&mut self, top: u16, rows: Vec<String>) {
        for (i, row) in rows.into_iter().enumerate() {
            let Ok(offset) = u16::try_from(i) else {
                break;
            };
            self.rows.insert(top.saturating_add(offset), row);
        }
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
                let text = self.rows.get(&row)?;
                let width = text.chars().count();
                let from = if row == start.0 {
                    char_at_cell(text, usize::from(start.1))
                } else {
                    0
                };
                // The release column is inclusive: a drag that stops on a
                // character has selected it, which is what the highlight under
                // the pointer says.
                let to = if row == end.0 {
                    (char_at_cell(text, usize::from(end.1)) + 1).min(width)
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
            .filter_map(|(row, from, to)| {
                let text = self.rows.get(&row)?;
                let cut: String = text.chars().take(to).skip(from).collect();
                Some(cut.trim_end().to_string())
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

/// The `char` index of the glyph occupying screen `cell` in `text`, clamped to
/// the row's length. A wide glyph occupies two cells; either one resolves to
/// its single `char` index, which is what keeps a drag that starts or ends on
/// the second cell of a CJK character from cutting into the wrong character.
fn char_at_cell(text: &str, cell: usize) -> usize {
    let mut width = 0usize;
    for (index, ch) in text.chars().enumerate() {
        let w = ch.width().unwrap_or(0).max(1);
        if cell < width + w {
            return index;
        }
        width += w;
    }
    text.chars().count()
}
