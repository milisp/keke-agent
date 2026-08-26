//! The one-row turn status line, shown between the transcript and the composer
//! only while a turn is on the clock — hidden (zero height) when idle.
//!
//! Layout: `⠋ working · 1m10s · ⇣43.5k`
//! - Spinner plus state label on the left
//! - Elapsed time beside it, compacted (`12s`, `1m10s`, `1h10m`, `1d3h`)
//! - Used context tokens just past the clock, where the eye already is; the
//!   window figure stays in the header

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

    // Used tokens only, inline after the clock rather than right-aligned:
    // at the screen edge nobody reads them mid-turn. No window figure here —
    // that stays the header's job. Absent until some input exists, so a fresh
    // turn does not open with a dangling separator.
    let used = app.context_input();
    let context = if used > 0 {
        format!("· ⇣{}", tokens(used))
    } else {
        String::new()
    };

    // Idle keeps the row as a quiet record of the finished turn; a fresh
    // session with no turn yet has no row at all.
    if !app.turn().is_busy() {
        let line = Line::from(vec![
            Span::styled("✓ done ", Style::new().fg(Color::Green)),
            Span::styled(
                format!("· worked for {} ", format_duration(elapsed)),
                Style::new().fg(Color::DarkGray),
            ),
            Span::styled(context, Style::new().fg(Color::Cyan)),
        ]);
        frame.render_widget(ratatui::widgets::Paragraph::new(line), area);
        return;
    }
    let label = match app.turn() {
        crate::app::Turn::AwaitingPermission => ("waiting for approval", Color::Yellow),
        _ => ("working", Color::Magenta),
    };
    let frame_index = usize::try_from(elapsed.as_millis() / 120).unwrap_or(0);
    let spinner = SPINNER[frame_index % SPINNER.len()];
    let line = Line::from(vec![
        Span::styled(format!("{spinner} {} ", label.0), Style::new().fg(label.1)),
        Span::styled(
            format!("· {} ", format_duration(elapsed)),
            Style::new().fg(Color::DarkGray),
        ),
        Span::styled(context, Style::new().fg(Color::Cyan)),
    ]);
    frame.render_widget(ratatui::widgets::Paragraph::new(line), area);
}
