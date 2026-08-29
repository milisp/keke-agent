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

/// `842`, `12.3k`, `1.2M`. Thousands once past four digits, so the number keeps
/// a stable width while a turn runs and does not jitter the bar around it.
pub(crate) fn tokens(count: u64) -> String {
    match count {
        0..10_000 => count.to_string(),
        10_000..1_000_000 => format!("{:.1}k", count as f64 / 1_000.0),
        _ => format!("{:.1}M", count as f64 / 1_000_000.0),
    }
}

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
    // Which route is serving. Shown beside the model because one vendor can be
    // registered twice — a subscription login and an API key — and then the
    // model id alone does not say which of them the answer came from.
    if let Some(provider) = app.provider() {
        spans.push(Span::styled(
            format!("· {provider} "),
            Style::new().fg(Color::Cyan),
        ));
    }
    // Which model is answering. A person switching between vendors mid-session
    // is choosing what the next answer costs and how good it will be; a bar
    // that does not say leaves them re-reading the transcript to find out.
    if !app.model().is_empty() {
        spans.push(Span::styled(
            format!("· {} ", app.model()),
            Style::new().fg(Color::Cyan),
        ));
    }
    // Beside the mode for the same reason: the level changes what the next
    // answer costs, and a person who cannot see it has to guess.
    if let Some(level) = app.reasoning_effort() {
        spans.push(Span::styled(
            format!("· {level} "),
            Style::new().fg(Color::Blue),
        ));
    }
    // Whatever keke just did on this person's behalf, briefly. Last, because
    // it is the only thing here that is news rather than state.
    if let Some(flash) = app.flash() {
        spans.push(Span::styled(
            format!("· {flash} "),
            Style::new().fg(Color::Green),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_stay_exact_until_the_column_would_jitter() {
        assert_eq!(tokens(842), "842");
        assert_eq!(tokens(12_345), "12.3k");
        assert_eq!(tokens(1_234_567), "1.2M");
        assert_eq!(tokens(12_345_678), "12.3M");
    }
}
