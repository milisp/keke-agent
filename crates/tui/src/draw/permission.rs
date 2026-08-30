//! The panel under the transcript while a tool call waits on approval.
//!
//! The request itself is not a scrollback cell — see
//! `crate::transcript::PermissionCell` — so this is the only place it is
//! drawn: name, summary, reason, and the choice. It takes over the composer's
//! row rather than sitting beside it, the same trade the plan panel makes
//! while a plan waits.

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
use crate::app::Turn;

/// Name + summary, the reason, the hint line, plus the top border.
const CHROME: u16 = 4;

pub(crate) fn rows(app: &App) -> u16 {
    if app.turn() != Turn::AwaitingPermission {
        return 0;
    }
    CHROME
}

pub(crate) fn draw(frame: &mut Frame, area: Rect, app: &App) {
    if app.turn() != Turn::AwaitingPermission {
        return;
    }
    let Some(prompt) = app.open_permission() else {
        return;
    };
    if area.height < CHROME || area.width < 24 {
        return;
    }

    let lines = vec![
        Line::from(vec![
            Span::styled(
                "? approve ",
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                prompt.name.clone(),
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(prompt.summary.clone(), Style::new().fg(Color::Gray)),
        ]),
        Line::styled(prompt.reason.clone(), Style::new().fg(Color::Gray)),
        hints(),
    ];

    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::TOP)),
        area,
    );
}

fn hints() -> Line<'static> {
    let mut spans = Vec::new();
    for (key, label) in [("y", "allow"), ("a", "always allow"), ("n", "deny")] {
        spans.push(Span::styled(
            format!("  {key} "),
            Style::new()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {label}"),
            Style::new().fg(Color::Gray),
        ));
    }
    Line::from(spans)
}
