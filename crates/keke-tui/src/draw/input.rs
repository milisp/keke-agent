//! The prompt box.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::Block;
use ratatui::widgets::Borders;
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::app::Turn;

/// Grow with the text, but never past this, so the transcript keeps the screen.
pub(crate) const MAX_ROWS: u16 = 8;

pub(crate) fn rows(app: &App) -> u16 {
    let used = u16::try_from(app.input.lines().len()).unwrap_or(MAX_ROWS);
    used.clamp(1, MAX_ROWS) + 2
}

pub(crate) fn draw(frame: &mut Frame, area: Rect, app: &App) {
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

    // A prompt longer than the box scrolls inside it, anchored so the cursor
    // is always on screen: text that has been typed but cannot be seen is
    // worse than a box that grew.
    let (row, column) = app.input.cursor_display();
    let visible = usize::from(rows(app).saturating_sub(2));
    let first = row.saturating_sub(visible.saturating_sub(1));
    let lines: Vec<Line> = app
        .input
        .lines()
        .iter()
        .skip(first)
        .take(visible)
        .map(Line::raw)
        .collect();
    frame.render_widget(Paragraph::new(lines).block(block), area);

    // No cursor while blocked: the keyboard is answering the prompt, and a
    // blinking caret in a box that ignores letters is a lie.
    if !blocked {
        let x = inner.x + u16::try_from(column).unwrap_or(0);
        let y = inner.y + u16::try_from(row - first).unwrap_or(0);
        if x < inner.right() && y < inner.bottom() {
            frame.set_cursor_position((x, y));
        }
    }
}
