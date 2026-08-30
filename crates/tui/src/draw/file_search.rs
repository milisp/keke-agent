//! The `@`-file/folder completion dropdown.
//!
//! Drawn above the composer, same as the slash-command menu: it belongs to
//! what is being typed, not to the transcript underneath it.

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

/// How tall the dropdown is this frame, borders included. Zero when closed.
pub(crate) fn rows(app: &App) -> u16 {
    let count = app.file_search.results().len();
    if count == 0 {
        return 0;
    }
    u16::try_from(count.min(MAX_ROWS)).unwrap_or(1) + 2
}

pub(crate) fn draw(frame: &mut Frame, area: Rect, app: &App) {
    let entries = app.file_search.results();
    if entries.is_empty() {
        return;
    }
    let selected = app.file_search.selected();
    let dir_mode = app.file_search.is_dir_mode();
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
            let path = entry.path.to_string();
            let suffix = if entry.is_dir { "/" } else { "" };
            Span::styled(
                format!(" {path}{suffix} "),
                style.add_modifier(Modifier::BOLD),
            )
            .into()
        })
        .collect();

    let title = if dir_mode {
        " folders — tab completes, enter drills in "
    } else {
        " files — tab completes, enter inserts "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::Cyan))
        .title(title);
    frame.render_widget(Paragraph::new(lines).block(block), area);
}
