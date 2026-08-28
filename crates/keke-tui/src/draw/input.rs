//! The prompt box.
//!
//! Wrapping happens here rather than in `Paragraph::wrap`, same reasoning as
//! the transcript: a widget that wraps internally does not say how many rows
//! it produced, and both the box's height and the cursor's on-screen position
//! need that count before anything is drawn.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::Block;
use ratatui::widgets::Borders;
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthChar as _;

use crate::app::App;
use crate::app::Turn;

/// Grow with the text, but never past this, so the transcript keeps the screen.
pub(crate) const MAX_ROWS: u16 = 8;

pub(crate) fn rows(app: &App, area_width: u16) -> u16 {
    let width = usize::from(area_width.saturating_sub(2)).max(1);
    let used: usize = app
        .input
        .lines()
        .iter()
        .map(|line| wrap_cells(line, width).len())
        .sum();
    u16::try_from(used).unwrap_or(MAX_ROWS).clamp(1, MAX_ROWS) + 2
}

pub(crate) fn draw(frame: &mut Frame, area: Rect, app: &mut App) {
    let blocked = app.turn() == Turn::AwaitingPermission;
    let border = if blocked {
        Style::new().fg(Color::Yellow)
    } else {
        Style::new().fg(Color::DarkGray)
    };
    let title = if blocked {
        " answer the prompt above "
    } else {
        " message "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border)
        .title(title);
    let inner = block.inner(area);
    let width = usize::from(inner.width).max(1);

    // Wrap every logical line to the box's visible width, and track where
    // that puts the cursor: the row it lands on is wherever its own logical
    // line's wrapped chunks put it, not the logical row index.
    let (cursor_row, cursor_column) = app.input.cursor_display();
    let mut display: Vec<String> = Vec::new();
    let mut cursor_display_row = 0usize;
    let mut cursor_display_column = cursor_column;
    for (index, line) in app.input.lines().iter().enumerate() {
        if index == cursor_row {
            let (offset, column) = wrap_position(line, cursor_column, width);
            cursor_display_row = display.len() + offset;
            cursor_display_column = column;
        }
        display.extend(wrap_cells(line, width));
    }

    // A prompt longer than the box scrolls inside it, anchored so the cursor
    // is always on screen: text that has been typed but cannot be seen is
    // worse than a box that grew.
    let visible = usize::from(area.height.saturating_sub(2));
    let first = cursor_display_row.saturating_sub(visible.saturating_sub(1));
    let shown: Vec<String> = display.iter().skip(first).take(visible).cloned().collect();
    // The drag is answered against what was drawn, so the frame hands the
    // selection its own rows before asking it to mark them — same contract
    // the transcript body follows in `draw::draw`.
    app.selection.add_rows(inner.y, shown.clone());
    let lines: Vec<Line> = shown
        .into_iter()
        .enumerate()
        .map(|(row, text)| {
            let row = u16::try_from(row)
                .unwrap_or(u16::MAX)
                .saturating_add(inner.y);
            app.selection.highlight(row, Line::raw(text))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).block(block), area);

    // No cursor while blocked: the keyboard is answering the prompt, and a
    // blinking caret in a box that ignores letters is a lie.
    if !blocked {
        let x = inner.x + u16::try_from(cursor_display_column).unwrap_or(0);
        let y = inner.y + u16::try_from(cursor_display_row - first).unwrap_or(0);
        if x < inner.right() && y < inner.bottom() {
            frame.set_cursor_position((x, y));
        }
    }
}

/// Break `line` into rows that each fit in `width` screen cells, without
/// splitting a double-width glyph across two rows. An empty line still
/// produces one (empty) row, matching how the unwrapped box always showed a
/// blank line as a row rather than nothing.
fn wrap_cells(line: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![line.to_string()];
    }
    let mut rows = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for ch in line.chars() {
        let w = ch.width().unwrap_or(0).max(1);
        if current_width > 0 && current_width + w > width {
            rows.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push(ch);
        current_width += w;
    }
    rows.push(current);
    rows
}

/// Where a cursor at cell `column` of `line` lands once `line` is wrapped to
/// `width`: the wrapped row offset within the line, and the cell column
/// within that row. Mirrors [`wrap_cells`]'s break points exactly, so the
/// caret always sits over the character it is actually before.
fn wrap_position(line: &str, column: usize, width: usize) -> (usize, usize) {
    if width == 0 {
        return (0, column);
    }
    let mut row = 0usize;
    let mut row_width = 0usize;
    let mut consumed = 0usize;
    for ch in line.chars() {
        if consumed >= column {
            break;
        }
        let w = ch.width().unwrap_or(0).max(1);
        if row_width > 0 && row_width + w > width {
            row += 1;
            row_width = 0;
        }
        row_width += w;
        consumed += w;
    }
    (row, row_width)
}
