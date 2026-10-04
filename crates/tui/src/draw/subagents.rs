//! Compact subagent rows below the status bar. Opening a row shows the
//! delegated task and its recorded transcript.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

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
    u16::try_from(
        app.subagents()
            .iter()
            .filter(|agent| agent.status.is_none())
            .count()
            .min(MAX_ROWS),
    )
    .unwrap_or(0)
}

/// Keep delegated work recognizable without turning its instructions into a
/// second transcript. The complete task remains available in the detail view.
fn title(task: &str) -> String {
    let subject = task
        .split_whitespace()
        .take(4)
        .collect::<Vec<_>>()
        .join(" ");
    shorten(&subject, 24)
}

fn shorten(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut prefix = String::new();
    for ch in text.chars() {
        if prefix.width() + unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) > width - 1 {
            break;
        }
        prefix.push(ch);
    }
    if let Some(boundary) = prefix.rfind(char::is_whitespace) {
        prefix.truncate(boundary);
    }
    format!("{}…", prefix.trim_end())
}

pub(crate) fn draw(frame: &mut Frame, area: Rect, app: &mut App) {
    if area.height == 0 || area.width < MIN_WIDTH {
        app.set_subagent_rows(Vec::new());
        return;
    }

    let mut hits = Vec::new();
    let mut lines = Vec::new();
    for (index, agent) in app
        .subagents()
        .iter()
        .filter(|agent| agent.status.is_none())
        .take(MAX_ROWS)
        .enumerate()
    {
        let (mark, colour) = ("☐", Color::Magenta);
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
        let width = usize::from(area.width);
        let room = width.saturating_sub(right.width() + 5);
        let left = format!(
            " {mark} {} ",
            shorten(&title(agent.title.as_deref().unwrap_or(&agent.id)), room)
        );
        let gap = width.saturating_sub(left.width() + right.width() + 1);

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

/// Identify the child while the shared transcript surface displays its messages.
pub(crate) fn navigation(frame: &mut Frame, header: Rect, footer: Rect, app: &App) {
    let Some(agent) = app.open_subagent() else {
        return;
    };
    let heading = format!(
        " {} · {} · {}",
        agent.id,
        title(agent.title.as_deref().unwrap_or(&agent.id)),
        agent.status.as_deref().unwrap_or("running")
    );
    frame.render_widget(ratatui::widgets::Clear, header);
    frame.render_widget(
        Paragraph::new(heading).style(Style::new().fg(Color::Cyan)),
        header,
    );
    frame.render_widget(ratatui::widgets::Clear, footer);
    frame.render_widget(
        Paragraph::new(" Ctrl+O full/compact · Ctrl+G next agent · Esc/q back ")
            .style(Style::new().fg(Color::DarkGray)),
        footer,
    );
}

/// History is an explicit picker; finished children never occupy live status rows.
pub(crate) fn history(frame: &mut Frame, app: &mut App) {
    let Some(selected) = app.subagent_history else {
        return;
    };
    let area = frame.area();
    let width = area.width.saturating_sub(4).min(72);
    let height = area.height.saturating_sub(4).min(
        u16::try_from(app.subagents().len())
            .unwrap_or(u16::MAX)
            .saturating_add(3),
    );
    if width < 4 || height < 4 {
        app.set_subagent_rows(Vec::new());
        return;
    }
    let popup = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    let block = ratatui::widgets::Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .title(" Subagents ");
    let inner = block.inner(popup);
    let count = usize::from(inner.height.saturating_sub(1));
    let start = selected.saturating_sub(count.saturating_sub(1));
    let mut hits = Vec::new();
    let lines: Vec<_> = app
        .subagents()
        .iter()
        .enumerate()
        .skip(start)
        .take(count)
        .map(|(index, agent)| {
            let row = inner.y + u16::try_from(index - start).unwrap_or(0);
            hits.push((row, agent.id.clone()));
            let label = format!(
                "{} {} · {} · {}",
                if index == selected { "›" } else { " " },
                title(agent.title.as_deref().unwrap_or(&agent.id)),
                agent.id,
                agent.status.as_deref().unwrap_or("running")
            );
            Line::styled(
                shorten(&label, usize::from(inner.width)),
                if index == selected {
                    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                } else {
                    Style::new()
                },
            )
        })
        .collect();
    app.set_subagent_rows(hits);
    frame.render_widget(ratatui::widgets::Clear, popup);
    frame.render_widget(block, popup);
    frame.render_widget(Paragraph::new(lines), inner);
    frame.render_widget(
        Paragraph::new(" ↑↓ select · Enter/click open · Esc close ")
            .style(Style::new().fg(Color::DarkGray)),
        Rect {
            y: inner.bottom() - 1,
            height: 1,
            ..inner
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_limits_its_title_to_four_words() {
        assert_eq!(
            title("find the parser\n\nlook in crates/"),
            "find the parser look"
        );
        assert_eq!(title("  padded  "), "padded");
    }

    #[test]
    fn long_titles_stay_short_even_in_a_wide_terminal() {
        let text = "Investigate the parser behavior and implement all necessary fixes across the workspace";
        assert_eq!(title(text), "Investigate the parser…");
        assert!(title(text).width() <= 24);
        assert_eq!(title("\n  Find parser  \nDetails"), "Find parser Details");
    }

    #[test]
    fn narrow_titles_respect_display_columns() {
        assert_eq!(shorten("解析器解析器", 7), "解析器…");
        assert_eq!(shorten("find the parser", 10), "find the…");
        assert_eq!(shorten("task", 0), "");
    }
}
