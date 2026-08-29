//! The panel under the transcript while a plan waits for its answer.
//!
//! The plan itself is a cell in the scrollback, not a window over it. What is
//! left to draw is three rows and a hint line: how much of it may run without
//! being asked, or whether to send it back with notes.

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
use crate::app::plan::PlanFocus;
use crate::app::plan::ROWS;

/// One row per choice, plus the hint bar and two borders.
const CHROME: u16 = 3;

/// How tall the panel is this frame. Zero when no plan is waiting.
pub(crate) fn rows(app: &App) -> u16 {
    if app.plan_review().is_none() {
        return 0;
    }
    u16::try_from(ROWS.len()).unwrap_or(3) + CHROME
}

pub(crate) fn draw(frame: &mut Frame, area: Rect, app: &App) {
    let Some(review) = app.plan_review() else {
        return;
    };
    if area.height < CHROME + 1 || area.width < 24 {
        return;
    }
    let composing = review.focus() == PlanFocus::Composer;

    let mut lines: Vec<Line> = ROWS
        .into_iter()
        .map(|row| {
            let chosen = row == review.row();
            let style = if chosen && !composing {
                Style::new().fg(Color::Black).bg(Color::Cyan)
            } else if chosen {
                Style::new().fg(Color::Cyan)
            } else {
                Style::new()
            };
            Line::from(Span::styled(
                format!(" {} {} ", if chosen { "›" } else { " " }, row.label()),
                style.add_modifier(Modifier::BOLD),
            ))
        })
        .collect();
    lines.push(hints(composing));

    let title = if composing {
        " what should change? — enter sends the plan back, esc goes back "
    } else {
        " carry the plan out — \u{2191}\u{2193} picks, enter confirms "
    };
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::new().fg(if composing {
                    Color::Cyan
                } else {
                    Color::Yellow
                }))
                .title(title),
        ),
        area,
    );
}

fn hints(composing: bool) -> Line<'static> {
    let offered: Vec<(&str, &str)> = if composing {
        vec![("esc", "back to the plan")]
    } else {
        vec![("ctrl+g", "edit in vim"), ("esc", "cancel")]
    };
    let mut spans = Vec::new();
    for (key, label) in offered {
        spans.push(Span::styled(
            format!("  {key} "),
            Style::new().fg(Color::Black).bg(Color::Yellow),
        ));
        spans.push(Span::styled(
            format!(" {label}"),
            Style::new().fg(Color::Gray),
        ));
    }
    Line::from(spans)
}
