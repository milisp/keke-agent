//! The model / provider / MCP picker, docked between the composer and the
//! status line.
//!
//! Same shape as the slash menu: a layout slot that collapses to zero when
//! closed. It still holds the keyboard while open; it just no longer covers
//! the transcript.

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
use crate::picker::Picker;

/// At most this many rows are on screen at once, before the 50% height cap.
const MAX_ROWS: usize = 12;
/// How narrow the panel may be squeezed before it is not worth drawing.
const MIN_WIDTH: u16 = 24;
/// Filter line + two borders. A panel shorter than this plus one list row
/// cannot be read.
const CHROME: u16 = 3;

/// One row as the overlay needs it: what a person reads, whatever else is
/// worth saying about it, and whether it is the one in force. Built here
/// because the two lists are drawn the same way and differ only in what fills
/// these three fields.
struct Row {
    current: bool,
    label: String,
    detail: String,
}

fn content(app: &App) -> Option<(&Picker, &'static str, Vec<Row>)> {
    if let Some(picker) = app.model_picker() {
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
        return Some((
            picker,
            " models \u{2014} type to filter, enter switches, esc cancels ",
            rows,
        ));
    }
    if let Some(picker) = app.provider_picker() {
        let rows = app
            .picker_providers()
            .into_iter()
            .map(|route| Row {
                current: app.provider() == Some(route.route.as_str()),
                label: route.display_name.clone(),
                detail: route.route.clone(),
            })
            .collect::<Vec<_>>();
        return Some((
            picker,
            " providers \u{2014} type to filter, enter switches, esc cancels ",
            rows,
        ));
    }
    if let Some(picker) = app.mcp_picker() {
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
        return Some((
            picker,
            " mcp servers \u{2014} type to filter, enter signs in, esc closes ",
            rows,
        ));
    }
    if let Some(picker) = app.policy_picker() {
        let rows = app
            .picker_policies()
            .into_iter()
            .map(|policy| Row {
                current: policy == app.approval_policy(),
                label: crate::slash::policy_name(policy).to_string(),
                detail: crate::picker::policy_detail(policy).to_string(),
            })
            .collect::<Vec<_>>();
        return Some((
            picker,
            " carry the plan out \u{2014} enter approves under this policy, esc keeps planning ",
            rows,
        ));
    }
    None
}

/// How tall the picker is this frame, borders and filter line included.
/// Zero when it is closed, or when the 50% cap would leave it unreadable.
pub(crate) fn rows(app: &App, height: u16) -> u16 {
    let Some((_, _, rows)) = content(app) else {
        return 0;
    };
    let wanted = u16::try_from(rows.len().clamp(1, MAX_ROWS)).unwrap_or(1) + CHROME;
    let capped = wanted.min(height / 2);
    if capped < CHROME + 1 { 0 } else { capped }
}

pub(crate) fn draw(frame: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 {
        return;
    }
    let Some((picker, title, rows)) = content(app) else {
        return;
    };
    if area.width < MIN_WIDTH {
        return;
    }
    let selected = app.picker_selected();
    let visible = usize::from(area.height.saturating_sub(CHROME)).max(1);
    let first = selected.saturating_sub(visible.saturating_sub(1));

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
    for (at, row) in rows.iter().enumerate().skip(first).take(visible) {
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
    frame.render_widget(Paragraph::new(lines).block(block), area);
}
