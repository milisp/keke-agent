//! Turning cells into lines.
//!
//! Wrapping happens here rather than in `Paragraph::wrap` because the
//! scrollback needs a line count it can pin a viewport to, and a widget that
//! wraps internally will not tell anyone how many lines it produced.

use keke_acp::PermissionAnswer;
use keke_protocol::ToolStatus;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use crate::transcript::CallState;
use crate::transcript::Cell;
use crate::transcript::PermissionCell;
use crate::transcript::ToolCell;

const USER: Color = Color::Cyan;
const THINKING: Color = Color::DarkGray;
const NOTICE: Color = Color::Blue;
const FAILURE: Color = Color::Red;
const DENIED: Color = Color::Yellow;
const SUCCESS: Color = Color::Green;

pub(crate) fn render(cells: &[Cell], width: u16, show_thinking: bool) -> Vec<Line<'static>> {
    let width = usize::from(width.max(8));
    let mut lines = Vec::new();
    for cell in cells {
        match cell {
            Cell::User(text) => {
                push_block(&mut lines, "› ", text, Style::new().fg(USER), width);
            }
            Cell::Assistant(text) => {
                push_block(&mut lines, "", text, Style::new(), width);
            }
            Cell::Thinking(text) if show_thinking => {
                let style = Style::new().fg(THINKING).add_modifier(Modifier::ITALIC);
                push_block(&mut lines, "  ", text, style, width);
            }
            Cell::Thinking(_) => continue,
            Cell::Tool(tool) => lines.extend(tool_lines(tool, width)),
            Cell::Permission(prompt) => lines.extend(permission_lines(prompt, width)),
            Cell::Error(message) => {
                let style = Style::new().fg(FAILURE);
                push_block(&mut lines, "error: ", message, style, width);
            }
            Cell::Notice(message) => {
                push_block(&mut lines, "· ", message, Style::new().fg(NOTICE), width);
            }
        }
        lines.push(Line::default());
    }
    lines
}

fn tool_lines(tool: &ToolCell, width: usize) -> Vec<Line<'static>> {
    let (marker, style) = match tool.state {
        CallState::Running => ("…", Style::new().fg(Color::Magenta)),
        // A denial is a decision, not a fault: it must not read like a crash.
        CallState::Finished(ToolStatus::Ok) => ("✓", Style::new().fg(SUCCESS)),
        CallState::Finished(ToolStatus::Error) => ("✗", Style::new().fg(FAILURE)),
        CallState::Finished(ToolStatus::Denied) => ("⊘", Style::new().fg(DENIED)),
        CallState::Finished(ToolStatus::Cancelled) => ("–", Style::new().fg(THINKING)),
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("{marker} "), style),
        Span::styled(tool.name.clone(), style.add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::styled(tool.summary.clone(), Style::new().fg(THINKING)),
    ])];
    if let Some(detail) = &tool.detail {
        push_block(&mut lines, "    ", detail, Style::new().fg(THINKING), width);
    }
    lines
}

fn permission_lines(prompt: &PermissionCell, width: usize) -> Vec<Line<'static>> {
    let (marker, style) = match prompt.answer {
        None => ("?", Style::new().fg(DENIED).add_modifier(Modifier::BOLD)),
        Some(PermissionAnswer::Allow | PermissionAnswer::AllowAlways) => {
            ("✓", Style::new().fg(SUCCESS))
        }
        Some(PermissionAnswer::Deny) => ("⊘", Style::new().fg(DENIED)),
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("{marker} approve "), style),
        Span::styled(prompt.name.clone(), style.add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::styled(prompt.summary.clone(), Style::new().fg(THINKING)),
    ])];
    push_block(
        &mut lines,
        "    ",
        &prompt.reason,
        Style::new().fg(THINKING),
        width,
    );
    lines.push(match prompt.answer {
        None => Line::styled(
            "    [y] allow  [a] always allow  [n] deny".to_string(),
            style,
        ),
        Some(answer) => Line::styled(format!("    answered: {}", label(answer)), style),
    });
    lines
}

fn label(answer: PermissionAnswer) -> &'static str {
    match answer {
        PermissionAnswer::Allow => "allowed",
        PermissionAnswer::AllowAlways => "always allowed",
        PermissionAnswer::Deny => "denied",
    }
}

/// Wrap `text` to `width`, prefixing the first line and indenting the rest so
/// a wrapped paragraph still reads as one block.
fn push_block(
    lines: &mut Vec<Line<'static>>,
    prefix: &str,
    text: &str,
    style: Style,
    width: usize,
) {
    let indent = " ".repeat(prefix.chars().count());
    let body = width.saturating_sub(prefix.chars().count()).max(1);
    let mut first = true;
    for paragraph in text.split('\n') {
        for chunk in wrap(paragraph, body) {
            let lead = if first { prefix } else { indent.as_str() };
            lines.push(Line::from(vec![
                Span::styled(lead.to_string(), style),
                Span::styled(chunk, style),
            ]));
            first = false;
        }
    }
}

/// Greedy word wrap, breaking inside a word only when it cannot fit alone.
fn wrap(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut used = 0;
    for word in text.split(' ') {
        let length = word.chars().count();
        if used > 0 && used + 1 + length > width {
            lines.push(std::mem::take(&mut current));
            used = 0;
        }
        if length > width {
            for ch in word.chars() {
                if used == width {
                    lines.push(std::mem::take(&mut current));
                    used = 0;
                }
                current.push(ch);
                used += 1;
            }
            continue;
        }
        if used > 0 {
            current.push(' ');
            used += 1;
        }
        current.push_str(word);
        used += length;
    }
    lines.push(current);
    lines
}
