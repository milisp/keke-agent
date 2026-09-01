//! The rewind overlay, docked where the picker is.
//!
//! A list of what the person said, newest first, so going back one turn — the
//! common case by far — is Esc Esc Enter and nothing else.

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

/// At most this many prompts are on screen at once, before the 50% height cap.
const MAX_ROWS: usize = 10;
/// Two borders. A panel shorter than this plus one row cannot be read.
const CHROME: u16 = 2;
/// How narrow the panel may be squeezed before it is not worth drawing.
const MIN_WIDTH: u16 = 24;

/// How tall the overlay is this frame. Zero when it is closed, or when the
/// height cap would leave it unreadable.
pub(crate) fn rows(app: &App, height: u16) -> u16 {
    let Some(rewind) = app.rewind() else {
        return 0;
    };
    let wanted = u16::try_from(rewind.points().len().clamp(1, MAX_ROWS)).unwrap_or(1) + CHROME;
    let capped = wanted.min(height / 2);
    if capped < CHROME + 1 { 0 } else { capped }
}

/// One prompt as a row: flattened, since a multi-line prompt still has to fit
/// on one, and numbered from the start of the conversation rather than from
/// the top of this list — the numbers are how a person finds their place in
/// what they scrolled through above.
fn label(turn: usize, text: &str) -> String {
    let flattened = text.split_whitespace().collect::<Vec<_>>().join(" ");
    format!(" {}. {flattened}", turn + 1)
}

pub(crate) fn draw(frame: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 || area.width < MIN_WIDTH {
        return;
    }
    let Some(rewind) = app.rewind() else {
        return;
    };
    let selected = rewind.selected();
    let visible = usize::from(area.height.saturating_sub(CHROME)).max(1);
    // Scroll only far enough to keep the highlight on screen: the list opens
    // at the newest prompt and most people never leave it.
    let first = selected.saturating_sub(visible.saturating_sub(1));

    let lines: Vec<Line> = rewind
        .points()
        .iter()
        .enumerate()
        .skip(first)
        .take(visible)
        .map(|(at, point)| {
            let style = if at == selected {
                Style::new()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            };
            Line::from(Span::styled(label(point.turn, &point.text), style))
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::Cyan))
        .title(" go back to \u{2014} enter rewinds and edits, esc cancels ");
    frame.render_widget(Paragraph::new(lines).block(block), area);
}
