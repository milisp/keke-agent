//! The one-line status bar.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::app::Turn;
use crate::keys;

pub(crate) fn draw(frame: &mut Frame, area: Rect, app: &App) {
    let (state, style) = match app.turn() {
        Turn::Idle => ("ready", Style::new().fg(Color::Green)),
        Turn::Running => ("working", Style::new().fg(Color::Magenta)),
        Turn::AwaitingPermission => (
            "blocked — awaiting approval",
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
    };

    let mut spans = vec![Span::styled(format!(" {state} "), style)];
    // Always shown, never only after a change: a person who cannot see the
    // current mode has to guess whether the next command will ask them.
    let policy = app.approval_policy();
    spans.push(Span::styled(
        format!("· {} ", crate::slash::policy_name(policy)),
        if policy == keke_config_types::ApprovalPolicy::Never {
            Style::new().fg(Color::Red)
        } else {
            Style::new().fg(Color::Cyan)
        },
    ));
    if !app.show_thinking() {
        spans.push(Span::styled(
            "· thinking hidden ",
            Style::new().fg(Color::DarkGray),
        ));
    }
    // Say so out loud: a reader who has scrolled up and stopped seeing new
    // output needs to know the output is still arriving.
    if !app.scroll.is_following() {
        spans.push(Span::styled(
            "· scrolled back (^L to follow) ",
            Style::new().fg(Color::Yellow),
        ));
    }
    spans.push(Span::styled(
        keys::hints(app.turn() == Turn::AwaitingPermission),
        Style::new().fg(Color::DarkGray),
    ));

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}
