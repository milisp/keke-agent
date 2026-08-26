//! The floating model overlay.
//!
//! Drawn last and over everything, on a cleared rect: it has the keyboard, so
//! it has to look like it does. Anything showing through it would read as
//! still typeable.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Block;
use ratatui::widgets::Borders;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;

use crate::app::App;

/// At most this many models are on screen at once.
const MAX_ROWS: usize = 12;
/// How wide the box gets, and how narrow it may be squeezed before it is not
/// worth drawing.
const MAX_WIDTH: u16 = 72;
const MIN_WIDTH: u16 = 24;

pub(crate) fn draw(frame: &mut Frame, app: &App) {
    let Some(picker) = app.model_picker() else {
        return;
    };
    let models = app.picker_models();
    let selected = app.picker_selected();

    let area = frame.area();
    let width = area.width.saturating_sub(4).min(MAX_WIDTH);
    let rows = u16::try_from(models.len().clamp(1, MAX_ROWS)).unwrap_or(1);
    // Rows, plus the filter line and the two borders.
    let height = rows + 3;
    if width < MIN_WIDTH || area.height < height {
        return;
    }
    let popup = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 3,
        width,
        height,
    };

    let first = selected.saturating_sub(MAX_ROWS - 1);
    let mut lines = vec![Line::from(vec![
        Span::styled(" filter ", Style::new().fg(Color::DarkGray)),
        Span::styled(
            picker.query().to_string(),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        Span::styled("▏", Style::new().fg(Color::Cyan)),
    ])];

    if models.is_empty() {
        lines.push(Line::styled(
            "  nothing matches — backspace to widen",
            Style::new().fg(Color::DarkGray),
        ));
    }
    for (at, model) in models.iter().enumerate().skip(first).take(MAX_ROWS) {
        let style = if at == selected {
            Style::new().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::new()
        };
        // The model in force is marked rather than moved to the top: a list
        // that reorders itself as you switch is one you cannot learn.
        let mark = if model.id == app.model() { "*" } else { " " };
        let mut spans = vec![Span::styled(
            format!(" {mark} {} ", model.display_name),
            style.add_modifier(Modifier::BOLD),
        )];
        let mut detail = model.id.clone();
        if let Some(window) = model.context_window {
            detail.push_str(&format!("  ·  {}k context", window / 1_000));
        }
        spans.push(Span::styled(
            detail,
            style.fg(if at == selected {
                Color::Black
            } else {
                Color::DarkGray
            }),
        ));
        lines.push(Line::from(spans));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::Cyan))
        .title(" models — type to filter, enter switches, esc cancels ");
    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}
