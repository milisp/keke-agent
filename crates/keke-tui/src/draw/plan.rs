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
use crate::app::plan::PlanFocus;

/// Borders plus the action bar and the blank line above it.
const CHROME: u16 = 4;

/// Width of the line-number gutter, including its trailing space.
const GUTTER: usize = 5;

/// The plan as rows, each tagged with the plan line it came from.
///
/// The preview is drawn one plan line at a time, numbered, rather than as
/// rendered markdown: a comment says "line 7", so line 7 has to be something a
/// person can see and point at. A line too long for the panel wraps, and every
/// row it wraps onto still belongs to it.
fn rows(text: &str, width: usize) -> Vec<(usize, String)> {
    let body = width.saturating_sub(GUTTER).max(8);
    text.lines()
        .enumerate()
        .flat_map(|(index, line)| {
            crate::draw::markdown::wrap_plain(line, body)
                .into_iter()
                .map(move |part| (index, part))
        })
        .collect()
}

/// What an empty plan says instead of a blank box.
///
/// An agent that left plan mode without writing anything is not an error and
/// not a blank box: a person can still approve and get on with it, so the
/// panel says what happened and keeps every action live.
fn empty_body() -> Vec<Line<'static>> {
    vec![
        Line::styled(
            "  The agent proposed no plan.",
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::styled(
            "  Approve to start building anyway, or send it back to say what to plan.",
            Style::new().fg(Color::DarkGray),
        ),
    ]
}

pub(crate) fn draw(frame: &mut Frame, area: Rect, app: &mut App) {
    let Some(review) = app.plan_review() else {
        return;
    };
    if area.height < CHROME + 3 || area.width < 24 {
        return;
    }

    let answered = review.is_answered();
    let commenting = review.is_commenting();
    let comments = review.comments().len();
    let focus = review.focus();
    let text = review.text().to_string();
    let (first, last) = review.selection();

    let width = area.width.saturating_sub(area.width / 8).max(24);
    let height = area.height.saturating_sub(area.height / 6).max(CHROME + 3);
    let popup = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };

    let inner = usize::from(width.saturating_sub(4));
    // A record has no composer: there is nothing left to say to a question
    // that has already been answered.
    let chrome = CHROME + u16::from(!answered);
    let visible = usize::from(height - chrome);

    let (shown, max) = if text.trim().is_empty() {
        (empty_body(), 0)
    } else {
        let all = rows(&text, inner);
        let max = all.len().saturating_sub(visible);
        app.clamp_plan_scroll(max);
        if let Some(row) = all.iter().position(|(index, _)| *index == first) {
            app.reveal_plan_row(row, visible);
        }
        let offset = app
            .plan_review()
            .map_or(0, |review| review.scroll())
            .min(max);
        let selected = focus == PlanFocus::Preview || commenting;
        let shown = all
            .iter()
            .skip(offset)
            .take(visible)
            .map(|(index, part)| row_line(*index, part, selected && (first..=last).contains(index)))
            .collect();
        (shown, max)
    };

    let offset = app
        .plan_review()
        .map_or(0, |review| review.scroll())
        .min(max);
    let mut shown = shown;
    while shown.len() < visible {
        shown.push(Line::raw(""));
    }
    if !answered {
        shown.push(composer(app, focus, commenting, first, last));
    }
    shown.push(Line::raw(""));
    shown.push(actions(offset < max, answered, comments));

    let title = if answered {
        " plan — answered, a record ".to_string()
    } else if max > 0 {
        format!(" plan — {} more lines below ", max - offset.min(max))
    } else {
        " plan ".to_string()
    };
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(shown).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::new().fg(if answered {
                    Color::DarkGray
                } else {
                    Color::Yellow
                }))
                .title(title),
        ),
        popup,
    );
}

fn row_line(index: usize, text: &str, selected: bool) -> Line<'static> {
    let gutter = format!("{:>4} ", index + 1);
    let body = Style::new();
    let body = if selected {
        body.bg(Color::DarkGray).add_modifier(Modifier::BOLD)
    } else {
        body
    };
    Line::from(vec![
        Span::styled(gutter, Style::new().fg(Color::DarkGray)),
        Span::styled(text.to_string(), body),
    ])
}

/// The one composer row inside the panel: what is being written, and about
/// what. Drawn here rather than left to the real composer because the overlay
/// covers the whole frame — a person typing must see their own words.
fn composer(
    app: &App,
    focus: PlanFocus,
    commenting: bool,
    first: usize,
    last: usize,
) -> Line<'static> {
    let label = if commenting && first == last {
        format!(" comment on line {} ", first + 1)
    } else if commenting {
        format!(" comment on lines {}-{} ", first + 1, last + 1)
    } else {
        " revision notes ".to_string()
    };
    let focused = focus == PlanFocus::Composer;
    let mut spans = vec![Span::styled(
        label,
        Style::new().fg(Color::Black).bg(if focused {
            Color::Cyan
        } else {
            Color::DarkGray
        }),
    )];
    let text = app.input.text().replace('\n', " ");
    if text.is_empty() && !focused {
        spans.push(Span::styled(
            "  tab to write",
            Style::new().fg(Color::DarkGray),
        ));
    } else {
        spans.push(Span::raw(format!("  {text}")));
        if focused {
            spans.push(Span::styled("▏", Style::new().fg(Color::Cyan)));
        }
    }
    Line::from(spans)
}

fn actions(more: bool, answered: bool, comments: usize) -> Line<'static> {
    let approve = if comments > 0 {
        "approve w/ comments"
    } else {
        "approve"
    };
    let offered: Vec<(&str, &str)> = if answered {
        vec![("y", "copy"), ("q", "close")]
    } else {
        vec![
            ("a", approve),
            ("s", "request changes"),
            ("c", "comment"),
            ("y", "copy"),
            ("q", "quit plan"),
        ]
    };
    let mut spans = Vec::new();
    for (key, label) in offered {
        spans.push(Span::styled(
            format!("  {key} "),
            Style::new().fg(Color::Black).bg(if answered {
                Color::DarkGray
            } else {
                Color::Yellow
            }),
        ));
        spans.push(Span::styled(
            format!(" {label}"),
            Style::new().fg(Color::Gray),
        ));
    }
    if more {
        spans.push(Span::styled(
            "   ↓ j/k moves",
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
        let lines = empty_body();
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|span| span.content.contains("no plan"))
        );
    }

    /// A wrapped line is still the line it was, or a comment on it would name
    /// a number the person never saw.
    #[test]
    fn wrapped_rows_keep_the_plan_line_they_came_from() {
        let rows = rows("short\nthis one is rather long indeed", 20);
        assert_eq!(rows[0].0, 0);
        assert!(rows.len() > 2);
        assert!(rows[1..].iter().all(|(index, _)| *index == 1));
    }

    /// The action bar has to say that comments are riding along, or a person
    /// cannot tell an approval from an approval-with-remarks.
    #[test]
    fn the_approve_action_says_when_comments_go_with_it() {
        let bar = actions(false, false, 2);
        let text: String = bar.spans.iter().map(|span| span.content.as_ref()).collect();
        assert!(text.contains("approve w/ comments"));
        assert!(
            !actions(false, true, 0)
                .spans
                .iter()
                .any(|span| span.content.contains("approve"))
        );
    }
}
