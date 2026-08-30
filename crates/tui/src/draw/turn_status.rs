//! The one-row turn status line, shown between the transcript and the composer
//! only while a turn is on the clock — hidden (zero height) when idle.
//!
//! Layout: `⠋ working · 1m10s · ⇣43.5k · thinking with medium effort`
//! - Spinner, state label, and elapsed time, compacted
//!   (`12s`, `1m10s`, `1h10m`, `1d3h`)
//! - Context tokens right after the clock, where the eye already is
//! - While the model is reasoning, the effort level trails the tokens as one
//!   phrase ("thinking with medium effort") rather than leading the line

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use crate::app::App;
use crate::draw::status::tokens;
use crate::ported::grok_build::format_duration;

/// Braille spinner frames, slowed to roughly one frame per 120ms by the draw
/// tick, so the wheel reads as turning rather than buzzing.
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// One row while a turn holds the clock or while there is a last-turn time
/// worth showing; none before the first turn.
pub(crate) fn rows(app: &App) -> u16 {
    u16::from(app.turn().is_busy() || app.elapsed().is_some())
}

pub(crate) fn draw(frame: &mut Frame, area: Rect, app: &App) {
    let elapsed = app.elapsed().unwrap_or_default();

    // Used tokens only, not the window figure — that stays the header's job.
    // Absent until some input exists, so a fresh turn does not open with a
    // dangling separator.
    let used = app.context_input();
    let context = if used > 0 {
        format!("⇣{}", tokens(used))
    } else {
        String::new()
    };

    // Idle keeps the row as a quiet record of the finished turn; a fresh
    // session with no turn yet has no row at all.
    if !app.turn().is_busy() {
        let done_at = app
            .last_turn_finished_at()
            .map(|at| format!(" · done {}", at.format("%-I:%M %p")))
            .unwrap_or_default();
        let mut spans = vec![Span::styled(
            format!("Cooked for {}{done_at}", format_duration(elapsed)),
            Style::new().fg(Color::DarkGray),
        )];
        if !context.is_empty() {
            spans.push(Span::styled(
                format!(" · {context}"),
                Style::new().fg(Color::Cyan),
            ));
        }
        frame.render_widget(ratatui::widgets::Paragraph::new(Line::from(spans)), area);
        return;
    }

    let label = match app.turn() {
        crate::app::Turn::AwaitingPermission => "waiting for approval",
        _ => "working",
    };
    let color = match app.turn() {
        crate::app::Turn::AwaitingPermission => Color::Yellow,
        _ if app.is_thinking() => Color::Cyan,
        _ => Color::Magenta,
    };
    let frame_index = usize::try_from(elapsed.as_millis() / 120).unwrap_or(0);
    let spinner = SPINNER[frame_index % SPINNER.len()];
    let mut spans = vec![
        Span::styled(format!("{spinner} {label} "), Style::new().fg(color)),
        Span::styled(
            format!("· {}", format_duration(elapsed)),
            Style::new().fg(Color::DarkGray),
        ),
    ];
    if !context.is_empty() {
        spans.push(Span::styled(
            format!(" · {context}"),
            Style::new().fg(Color::Cyan),
        ));
    }
    // Kept as one phrase — "thinking with medium effort" — trailing the
    // token count rather than leading the line, since the clock and tokens
    // are what the eye tracks turn over turn.
    if app.is_thinking() {
        spans.push(Span::styled(
            format!(
                " · thinking with {} effort",
                crate::slash::effort_name(app.reasoning_effort())
            ),
            Style::new().fg(Color::Cyan),
        ));
    }
    frame.render_widget(ratatui::widgets::Paragraph::new(Line::from(spans)), area);
}
