//! The slash-command completion menu.
//!
//! Drawn above the composer rather than over the transcript: it belongs to what
//! is being typed, and covering the answer a person is reading to decide what
//! to type next is the wrong trade.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Block;
use ratatui::widgets::Borders;
use ratatui::widgets::Paragraph;

use crate::app::App;

/// At most this many entries are on screen at once.
const MAX_ROWS: usize = 6;

/// How tall the menu is this frame, borders included. Zero when it is closed.
pub(crate) fn rows(app: &App) -> u16 {
    let count = app.completions().len();
    if count == 0 {
        return 0;
    }
    u16::try_from(count.min(MAX_ROWS)).unwrap_or(1) + 2
}

pub(crate) fn draw(frame: &mut Frame, area: Rect, app: &App) {
    let entries = app.completions();
    if entries.is_empty() {
        return;
    }
    let selected = app.completion();
    // Scroll with the highlight so the selection is never off the bottom of a
    // list longer than the window.
    let first = selected.saturating_sub(MAX_ROWS - 1);

    let lines: Vec<Line> = entries
        .iter()
        .enumerate()
        .skip(first)
        .take(MAX_ROWS)
        .map(|(at, entry)| {
            let style = if at == selected {
                Style::new().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::new()
            };
            Line::from(vec![
                Span::styled(
                    format!(" /{} ", entry.name),
                    style.add_modifier(Modifier::BOLD),
                ),
                Span::styled(entry.description.clone(), Style::new().fg(Color::DarkGray)),
            ])
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::Cyan))
        .title(" commands — tab completes, enter runs ");
    frame.render_widget(Paragraph::new(lines).block(block), area);
}
