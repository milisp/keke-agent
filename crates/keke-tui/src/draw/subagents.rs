//! The subagent rows, between the turn status and the prompt box.
//!
//! Delegated work is the one thing happening in a turn that leaves no trace in
//! the transcript until it is finished: `spawn_agent` returns a handle, and
//! then nothing is said for however long the child takes. These rows are that
//! trace. They are drawn where the turn status is rather than in the
//! transcript because they are not something the agent said — they are what it
//! is doing, and they stop being true.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::draw::status::tokens;
use crate::ported::grok_build::format_duration;

/// At most this many rows, so a model that starts a dozen subagents cannot
/// push the prompt box off the screen.
const MAX_ROWS: usize = 6;
/// Below this the row has no room for a title worth reading, so it is not
/// drawn at all rather than drawn as ellipses.
const MIN_WIDTH: u16 = 24;

pub(crate) fn rows(app: &App) -> u16 {
    u16::try_from(app.subagents().len().min(MAX_ROWS)).unwrap_or(0)
}

/// `☐` while it runs, `☑` when it finished cleanly, `☒` when it did not.
fn checkbox(status: Option<&str>) -> (&'static str, Color) {
    match status {
        None => ("☐", Color::Magenta),
        Some("completed") => ("☑", Color::Green),
        Some(_) => ("☒", Color::Red),
    }
}

/// The first line of the task, which is what a person wrote it as: an
/// instruction's first line is its subject, and the rest is detail that only
/// fits in the popup anyway.
fn title(task: &str) -> &str {
    task.lines().next().unwrap_or("").trim()
}

pub(crate) fn draw(frame: &mut Frame, area: Rect, app: &mut App) {
    if area.height == 0 || area.width < MIN_WIDTH {
        app.set_subagent_rows(Vec::new());
        return;
    }

    let mut hits = Vec::new();
    let mut lines = Vec::new();
    for (index, agent) in app.subagents().iter().take(MAX_ROWS).enumerate() {
        let (mark, colour) = checkbox(agent.status.as_deref());
        let elapsed = app.subagent_elapsed(&agent.id).unwrap_or_default();
        let right = if agent.input_tokens > 0 {
            format!(
                "⇣{} · {}",
                tokens(agent.input_tokens),
                format_duration(elapsed)
            )
        } else {
            format_duration(elapsed)
        };
        let left = format!(" {mark} {} ", title(&agent.task));

        // Justified: the title runs from the left and the cost from the right,
        // so the numbers stay in one column while the titles differ in length.
        let width = usize::from(area.width);
        let gap = width
            .saturating_sub(left.chars().count())
            .saturating_sub(right.chars().count() + 1);
        let left = if gap == 0 {
            // No room for both. The cost is the part that changes and the part
            // a person is watching, so the title is what gives way.
            let room = width.saturating_sub(right.chars().count() + 4);
            format!(
                " {mark} {}… ",
                left.chars().skip(3).take(room).collect::<String>()
            )
        } else {
            left
        };
        let gap = width
            .saturating_sub(left.chars().count())
            .saturating_sub(right.chars().count() + 1);

        lines.push(Line::from(vec![
            Span::styled(left, Style::new().fg(colour).add_modifier(Modifier::BOLD)),
            Span::raw(" ".repeat(gap)),
            Span::styled(format!("{right} "), Style::new().fg(Color::DarkGray)),
        ]));
        if let Ok(row) = u16::try_from(index) {
            hits.push((area.y + row, agent.id.clone()));
        }
    }

    app.set_subagent_rows(hits);
    frame.render_widget(Paragraph::new(lines), area);
}

/// The popup a click on a row opens: the whole task, which the row could only
/// show the first line of.
pub(crate) fn detail(frame: &mut Frame, app: &App) {
    let Some(agent) = app.open_subagent() else {
        return;
    };

    let area = frame.area();
    let width = area.width.saturating_sub(6).min(76);
    if width < MIN_WIDTH {
        return;
    }

    let inner = usize::from(width.saturating_sub(4));
    let mut body: Vec<Line> = Vec::new();
    for paragraph in agent.task.lines() {
        if paragraph.is_empty() {
            body.push(Line::raw(""));
            continue;
        }
        for chunk in wrap(paragraph, inner) {
            body.push(Line::raw(format!("  {chunk}")));
        }
    }

    let elapsed = app.subagent_elapsed(&agent.id).unwrap_or_default();
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!("  {} ", agent.id),
                Style::new().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "· {} · ⇣{} · {}",
                    agent.status.as_deref().unwrap_or("running"),
                    tokens(agent.input_tokens),
                    format_duration(elapsed)
                ),
                Style::new().fg(Color::DarkGray),
            ),
        ]),
        Line::raw(""),
    ];
    lines.append(&mut body);
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "  esc to close",
        Style::new().fg(Color::DarkGray),
    ));

    let height = u16::try_from(lines.len() + 2)
        .unwrap_or(u16::MAX)
        .min(area.height);
    let popup = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 3,
        width,
        height,
    };
    frame.render_widget(ratatui::widgets::Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(
            ratatui::widgets::Block::default()
                .borders(ratatui::widgets::Borders::ALL)
                .border_style(Style::new().fg(Color::Cyan))
                .title(" subagent "),
        ),
        popup,
    );
}

/// Break `text` on whitespace at `width` columns, keeping a word that is longer
/// than the line whole rather than losing part of a path or an identifier.
fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
            out.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_finished_subagent_is_marked_apart_from_one_that_failed() {
        assert_eq!(checkbox(None).0, "☐");
        assert_eq!(checkbox(Some("completed")).0, "☑");
        assert_eq!(checkbox(Some("timed_out")).0, "☒");
        assert_eq!(checkbox(Some("cancelled")).0, "☒");
    }

    /// The row shows the instruction's subject, not its first eighty
    /// characters: a task written over several lines would otherwise draw its
    /// preamble and never its point.
    #[test]
    fn a_row_titles_itself_with_the_first_line_of_the_task() {
        assert_eq!(
            title("find the parser\n\nlook in crates/"),
            "find the parser"
        );
        assert_eq!(title("  padded  "), "padded");
    }

    #[test]
    fn wrapping_keeps_a_word_longer_than_the_line_whole() {
        assert_eq!(wrap("a bb ccc", 4), vec!["a bb", "ccc"]);
        assert_eq!(
            wrap("crates/keke-subagent/src/host.rs", 8),
            vec!["crates/keke-subagent/src/host.rs"]
        );
    }
}
