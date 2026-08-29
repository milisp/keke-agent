//! The panel under the transcript while a plan waits for its answer.
//!
//! The plan itself is a cell in the scrollback, not a window over it. What is
//! left to draw is the one question the plan leaves open — how much of it may
//! be carried out without being asked — and the keys that answer it.

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

/// One row per policy, plus the action bar and two borders.
const CHROME: u16 = 3;

/// How tall the panel is this frame. Zero when no plan is waiting.
pub(crate) fn rows(app: &App) -> u16 {
    if app.plan_review().is_none() {
        return 0;
    }
    u16::try_from(crate::slash::POLICIES.len()).unwrap_or(3) + CHROME
}

pub(crate) fn draw(frame: &mut Frame, area: Rect, app: &App) {
    let Some(review) = app.plan_review() else {
        return;
    };
    if area.height < CHROME + 1 || area.width < 24 {
        return;
    }
    let composing = review.focus() == PlanFocus::Composer;

    let mut lines: Vec<Line> = crate::slash::POLICIES
        .into_iter()
        .map(|policy| {
            let chosen = policy == review.policy();
            let style = if chosen && !composing {
                Style::new().fg(Color::Black).bg(Color::Cyan)
            } else if chosen {
                Style::new().fg(Color::Cyan)
            } else {
                Style::new()
            };
            Line::from(vec![
                Span::styled(
                    format!(
                        " {} {} ",
                        if chosen { "›" } else { " " },
                        crate::slash::policy_name(policy)
                    ),
                    style.add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    crate::slash::policy_detail(policy),
                    if chosen && !composing {
                        style.fg(Color::Black)
                    } else {
                        Style::new().fg(Color::DarkGray)
                    },
                ),
            ])
        })
        .collect();
    lines.push(actions(review.comments().len(), composing));

    let title = if composing {
        match review.is_commenting() {
            true => " what should this say? — enter attaches it, esc goes back ",
            false => " what should change? — enter sends the plan back, esc goes back ",
        }
    } else {
        " carry the plan out — ↑↓ picks how much runs without asking "
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

fn actions(comments: usize, composing: bool) -> Line<'static> {
    let approve = if comments > 0 {
        "approve w/ comments"
    } else {
        "approve"
    };
    let offered: Vec<(&str, &str)> = if composing {
        vec![("esc", "back to the plan")]
    } else {
        vec![
            ("a", approve),
            ("s", "request changes"),
            ("c", "comment"),
            ("j/k", "move"),
            ("y", "copy"),
            ("q", "quit plan"),
        ]
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The action bar has to say that comments are riding along, or a person
    /// cannot tell an approval from an approval-with-remarks.
    #[test]
    fn the_approve_action_says_when_comments_go_with_it() {
        let bar = actions(2, false);
        let text: String = bar.spans.iter().map(|span| span.content.as_ref()).collect();
        assert!(text.contains("approve w/ comments"));
    }
}
