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

/// `12s`, `1m20s`, `1h02m`. Seconds until a minute, then minutes, because past
/// an hour the seconds are noise and the column would keep changing width.
fn duration(elapsed: std::time::Duration) -> String {
    let seconds = elapsed.as_secs();
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m{:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h{:02}m", seconds / 3600, (seconds % 3600) / 60),
    }
}

/// `842`, `12.3k`, `1.2M`. Thousands once past four digits, so the number keeps
/// a stable width while a turn runs and does not jitter the bar around it.
fn tokens(count: u64) -> String {
    match count {
        0..10_000 => count.to_string(),
        10_000..10_000_000 => format!("{:.1}k", count as f64 / 1_000.0),
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
    // Beside the mode for the same reason: the level changes what the next
    // answer costs, and a person who cannot see it has to guess.
    if let Some(level) = app.reasoning_effort() {
        spans.push(Span::styled(
            format!("· {level} "),
            Style::new().fg(Color::Blue),
        ));
    }
    // The two live numbers: how long this has been going, and what it has
    // cost. Shown while the turn runs — after it ends they answer "how long did
    // that take", which is the question a person asks once the answer is up.
    if let Some(elapsed) = app.elapsed() {
        let label = if app.turn().is_busy() {
            duration(elapsed)
        } else {
            format!("worked for {}", duration(elapsed))
        };
        spans.push(Span::styled(
            format!("· {label} "),
            Style::new().fg(Color::DarkGray),
        ));
    }
    let used = app.usage().total();
    if used > 0 {
        spans.push(Span::styled(
            format!("· {} tokens ", tokens(used)),
            Style::new().fg(Color::DarkGray),
        ));
    }
    if !app.show_thinking() {
        spans.push(Span::styled(
            "· thinking hidden ",
            Style::new().fg(Color::DarkGray),
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
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_duration_drops_seconds_once_it_stops_being_about_seconds() {
        assert_eq!(duration(Duration::from_secs(9)), "9s");
        assert_eq!(duration(Duration::from_secs(80)), "1m20s");
        assert_eq!(duration(Duration::from_secs(3_720)), "1h02m");
    }

    #[test]
    fn tokens_stay_exact_until_the_column_would_jitter() {
        assert_eq!(tokens(842), "842");
        assert_eq!(tokens(12_345), "12.3k");
        assert_eq!(tokens(12_345_678), "12.3M");
    }
}
