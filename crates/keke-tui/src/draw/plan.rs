//! The plan review: what the agent proposes, and the four things a person can
//! do about it.
//!
//! Drawn as an overlay rather than as a transcript cell because it is read
//! before it is answered — the turn is stopped on it, so it has to stay put
//! while somebody scrolls a page of prose rather than sliding up under new
//! output.

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

/// Borders plus the action bar and the blank line above it.
const CHROME: u16 = 4;

/// What the action bar offers, in the order a person is likeliest to want it.
const ACTIONS: [(&str, &str); 4] = [
    ("a", "approve"),
    ("s", "request changes"),
    ("y", "copy"),
    ("q", "quit plan"),
];

/// The plan as lines, or the empty state.
///
/// An agent that left plan mode without writing anything is not an error and
/// not a blank box: a person can still approve and get on with it, so the
/// panel says what happened and keeps every action live.
fn body(text: &str, width: usize) -> Vec<Line<'static>> {
    if text.trim().is_empty() {
        return vec![
            Line::styled(
                "  The agent proposed no plan.",
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Line::raw(""),
            Line::styled(
                "  Approve to start building anyway, or send it back to say what to plan.",
                Style::new().fg(Color::DarkGray),
            ),
        ];
    }
    crate::draw::markdown::render(text, width, Style::new(), "  ")
}

pub(crate) fn draw(frame: &mut Frame, area: Rect, app: &mut App) {
    let Some(review) = app.plan_review() else {
        return;
    };
    // Two rows of border and one of actions leave nothing to read below this.
    if area.height < CHROME + 2 || area.width < 24 {
        return;
    }

    let width = area.width.saturating_sub(area.width / 8).max(24);
    let height = area.height.saturating_sub(area.height / 6).max(CHROME + 2);
    let popup = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };

    let lines = body(review.text(), usize::from(width.saturating_sub(4)));
    let visible = usize::from(height - CHROME);
    let max = lines.len().saturating_sub(visible);
    app.clamp_plan_scroll(max);
    // Re-borrowed after the clamp: the offset drawn has to be the clamped one,
    // or the last key before a resize scrolls past the end.
    let offset = app.plan_review().map_or(0, |review| review.scroll());

    let mut shown: Vec<Line<'static>> = lines.into_iter().skip(offset).take(visible).collect();
    while shown.len() < visible {
        shown.push(Line::raw(""));
    }
    shown.push(Line::raw(""));
    shown.push(actions(offset < max));

    let title = if max > 0 {
        format!(" plan — {} more lines below ", max - offset.min(max))
    } else {
        " plan ".to_string()
    };
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(shown).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::new().fg(Color::Yellow))
                .title(title),
        ),
        popup,
    );
}

fn actions(more: bool) -> Line<'static> {
    let mut spans = Vec::new();
    for (key, label) in ACTIONS {
        spans.push(Span::styled(
            format!("  {key} "),
            Style::new().fg(Color::Black).bg(Color::Yellow),
        ));
        spans.push(Span::styled(
            format!(" {label}"),
            Style::new().fg(Color::Gray),
        ));
    }
    if more {
        spans.push(Span::styled(
            "   ↓ j/k scrolls",
            Style::new().fg(Color::DarkGray),
        ));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plan that was never written still gets a surface worth answering.
    #[test]
    fn an_empty_plan_says_so_rather_than_drawing_nothing() {
        let lines = body("   \n", 60);
        assert!(!lines.is_empty());
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|span| span.content.contains("no plan"))
        );
    }
}
