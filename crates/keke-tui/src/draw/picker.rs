//! The floating model and provider overlay.
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

/// At most this many rows are on screen at once.
const MAX_ROWS: usize = 12;
/// How wide the box gets, and how narrow it may be squeezed before it is not
/// worth drawing.
const MAX_WIDTH: u16 = 72;
const MIN_WIDTH: u16 = 24;

/// One row as the overlay needs it: what a person reads, whatever else is
/// worth saying about it, and whether it is the one in force. Built here
/// because the two lists are drawn the same way and differ only in what fills
/// these three fields.
struct Row {
    current: bool,
    label: String,
    detail: String,
}

pub(crate) fn draw(frame: &mut Frame, app: &App) {
    let (picker, title, rows) = if let Some(picker) = app.model_picker() {
        let rows = app
            .picker_models()
            .into_iter()
            .map(|model| {
                let mut detail = model.id.clone();
                if let Some(window) = model.context_window {
                    detail.push_str(&format!("  \u{b7}  {}k context", window / 1_000));
                }
                Row {
                    current: model.id == app.model(),
                    label: model.display_name.clone(),
                    detail,
                }
            })
            .collect::<Vec<_>>();
        (
            picker,
            " models \u{2014} type to filter, enter switches, esc cancels ",
            rows,
        )
    } else if let Some(picker) = app.provider_picker() {
        let rows = app
            .picker_providers()
            .into_iter()
            .map(|route| Row {
                current: app.provider() == Some(route.route.as_str()),
                label: route.display_name.clone(),
                detail: route.route.clone(),
            })
            .collect::<Vec<_>>();
        (
            picker,
            " providers \u{2014} type to filter, enter switches, esc cancels ",
            rows,
        )
    } else if let Some(picker) = app.mcp_picker() {
        let rows = app
            .picker_mcp()
            .into_iter()
            .map(|server| {
                // Trust comes before a token: a server that will not be reached
                // at all should not send anyone off to authenticate with it.
                // A login in flight is the freshest thing there is to say
                // about a row, so it displaces the standing status.
                let state = if let Some(activity) = app.mcp_activity(&server.name) {
                    activity.to_string()
                } else if !server.allowed {
                    format!("held back \u{2014} keke plugin trust {}", server.plugin)
                } else if !server.remote {
                    "local".to_string()
                } else if server.signed_in {
                    "signed in".to_string()
                } else {
                    "not signed in \u{2014} enter".to_string()
                };
                Row {
                    // The mark means "nothing to do here", which for a server
                    // is a reachable one that needs no token.
                    current: server.allowed && (!server.remote || server.signed_in),
                    label: server.name.clone(),
                    detail: format!("{}  \u{b7}  {state}", server.transport),
                }
            })
            .collect::<Vec<_>>();
        (
            picker,
            " mcp servers \u{2014} type to filter, enter signs in, esc closes ",
            rows,
        )
    } else {
        return;
    };
    let selected = app.picker_selected();

    let area = frame.area();
    let width = area.width.saturating_sub(4).min(MAX_WIDTH);
    let rows_shown = u16::try_from(rows.len().clamp(1, MAX_ROWS)).unwrap_or(1);
    // Rows, plus the filter line and the two borders.
    let height = rows_shown + 3;
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

    if rows.is_empty() {
        lines.push(Line::styled(
            "  nothing matches — backspace to widen",
            Style::new().fg(Color::DarkGray),
        ));
    }
    for (at, row) in rows.iter().enumerate().skip(first).take(MAX_ROWS) {
        let style = if at == selected {
            Style::new().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::new()
        };
        // The one in force is marked rather than moved to the top: a list that
        // reorders itself as you switch is one you cannot learn.
        let mark = if row.current { "*" } else { " " };
        let mut spans = vec![Span::styled(
            format!(" {mark} {} ", row.label),
            style.add_modifier(Modifier::BOLD),
        )];
        spans.push(Span::styled(
            row.detail.clone(),
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
        .title(title);
    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}
