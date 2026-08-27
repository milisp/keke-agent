//! Turning cells into lines.
//!
//! Wrapping happens here rather than in `Paragraph::wrap` because the
//! scrollback needs a line count it can pin a viewport to, and a widget that
//! wraps internally will not tell anyone how many lines it produced.

use std::collections::HashSet;

use keke_acp::PermissionAnswer;
use keke_protocol::ToolStatus;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use super::markdown;
use crate::transcript::CallState;
use crate::transcript::Cell;
use crate::transcript::PermissionCell;
use crate::transcript::ToolCell;
use crate::transcript::groups_with;
use crate::transcript::verb;

const USER: Color = Color::Cyan;
const THINKING: Color = Color::DarkGray;
const NOTICE: Color = Color::Blue;
const FAILURE: Color = Color::Red;
const DENIED: Color = Color::Yellow;
const SUCCESS: Color = Color::Green;

/// A drawn transcript, plus where its expandable headers landed.
///
/// A click arrives as a screen position and nothing else, so the frame that
/// drew a header is the only thing that can say which cell it belongs to.
#[derive(Debug, Default)]
pub(crate) struct Rendered {
    pub lines: Vec<Line<'static>>,
    /// `(line index, cell key)` for every row a click may expand or collapse.
    pub toggles: Vec<(usize, usize)>,
}

/// The status a group is reported as: the worst one in it.
///
/// A run summarised by its first call would hide a failure behind two
/// successes, which is the one thing a collapsed line must never do.
fn worst(status: ToolStatus, running: ToolStatus) -> ToolStatus {
    fn rank(status: ToolStatus) -> u8 {
        match status {
            ToolStatus::Ok => 0,
            ToolStatus::Cancelled => 1,
            ToolStatus::Denied => 2,
            ToolStatus::Error => 3,
        }
    }
    if rank(status) >= rank(running) {
        status
    } else {
        running
    }
}

pub(crate) fn render(
    cells: &[Cell],
    width: u16,
    show_thinking: bool,
    expanded: &HashSet<usize>,
) -> Rendered {
    let width = usize::from(width.max(8));
    let mut out = Rendered::default();
    let mut index = 0;
    while index < cells.len() {
        let cell = &cells[index];
        let mut spaced = true;
        match cell {
            Cell::User(text) => {
                push_block(&mut out.lines, "› ", text, Style::new().fg(USER), width);
            }
            Cell::Assistant(text) => {
                out.lines
                    .extend(markdown::render(text, width, Style::new(), ""));
            }
            // The cell still being streamed is the one being read, so it stays
            // open; a finished thought collapses to a line that says it
            // happened and can be opened again.
            Cell::Thinking(text) if show_thinking => {
                let style = Style::new().fg(THINKING).add_modifier(Modifier::ITALIC);
                let streaming = index + 1 == cells.len();
                if streaming || expanded.contains(&index) {
                    if !streaming {
                        out.toggles.push((out.lines.len(), index));
                        out.lines.push(header("✻", "Thought", "", true, style));
                    }
                    out.lines.extend(markdown::render(text, width, style, "  "));
                } else {
                    out.toggles.push((out.lines.len(), index));
                    let count = text.split('\n').count();
                    let noun = if count == 1 { "line" } else { "lines" };
                    let summary = format!("{count} {noun}");
                    out.lines
                        .push(header("✻", "Thought", &summary, false, style));
                }
            }
            Cell::Thinking(_) => spaced = false,
            Cell::Tool(tool) if matches!(tool.state, CallState::Running) => {
                out.lines.extend(tool_lines(tool, width));
            }
            Cell::Tool(first) => {
                let verb = verb(&first.name).0;
                let end = cells[index..]
                    .iter()
                    .position(|cell| !groups_with(cell, verb))
                    .map_or(cells.len(), |offset| index + offset);
                out.toggles.push((out.lines.len(), index));
                out.lines.extend(group_lines(
                    &cells[index..end],
                    expanded.contains(&index),
                    width,
                ));
                index = end;
                out.lines.push(Line::default());
                continue;
            }
            Cell::Permission(prompt) => out.lines.extend(permission_lines(prompt, width)),
            Cell::Error(message) => {
                let style = Style::new().fg(FAILURE);
                push_block(&mut out.lines, "error: ", message, style, width);
            }
            Cell::Notice(message) => {
                push_block(
                    &mut out.lines,
                    "· ",
                    message,
                    Style::new().fg(NOTICE),
                    width,
                );
            }
        }
        if spaced {
            out.lines.push(Line::default());
        }
        index += 1;
    }
    out
}

/// The one line a collapsed thing shows, with the marker that says which way
/// it opens.
fn header(marker: &str, title: &str, summary: &str, open: bool, style: Style) -> Line<'static> {
    let mut spans = vec![
        Span::styled(format!("{marker} "), style),
        Span::styled(title.to_string(), style.add_modifier(Modifier::BOLD)),
    ];
    if !summary.is_empty() {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(summary.to_string(), Style::new().fg(THINKING)));
    }
    spans.push(Span::styled(
        if open { "  ▾" } else { "  ▸" }.to_string(),
        Style::new().fg(THINKING),
    ));
    Line::from(spans)
}

/// One run of finished calls of the same kind, as a single header that opens.
///
/// Collapsed, a lone call still names what it acted on — `Read src/app.rs` —
/// because a count of one tells a reader nothing they did not already see.
fn group_lines(group: &[Cell], open: bool, width: usize) -> Vec<Line<'static>> {
    let tools: Vec<&ToolCell> = group
        .iter()
        .filter_map(|cell| match cell {
            Cell::Tool(tool) => Some(tool),
            _ => None,
        })
        .collect();
    let Some(first) = tools.first() else {
        return Vec::new();
    };
    let (verb, noun) = verb(&first.name);
    let status = tools
        .iter()
        .fold(ToolStatus::Ok, |worst_so_far, tool| match tool.state {
            CallState::Finished(status) => worst(status, worst_so_far),
            CallState::Running => worst_so_far,
        });
    let (marker, style) = marker(CallState::Finished(status));
    let summary = if tools.len() == 1 {
        first.summary.clone()
    } else {
        format!("{} {noun}", tools.len())
    };

    let mut lines = Vec::new();
    lines.push(header(marker, verb, &summary, open, style));
    if !open {
        return lines;
    }
    for tool in &tools {
        if tools.len() > 1 {
            let (glyph, style) = self::marker(tool.state);
            lines.push(Line::from(vec![
                Span::styled(format!("    {glyph} "), style),
                Span::styled(tool.summary.clone(), Style::new().fg(THINKING)),
            ]));
        }
        if tool.arguments != tool.summary {
            push_block(
                &mut lines,
                "      ",
                &tool.arguments,
                Style::new().fg(THINKING),
                width,
            );
        }
        if let Some(detail) = &tool.detail {
            push_block(
                &mut lines,
                "      ",
                detail,
                Style::new().fg(THINKING),
                width,
            );
        }
    }
    lines
}

/// The glyph and colour for a call's state.
fn marker(state: CallState) -> (&'static str, Style) {
    match state {
        CallState::Running => ("…", Style::new().fg(Color::Magenta)),
        CallState::Finished(ToolStatus::Ok) => ("✓", Style::new().fg(SUCCESS)),
        CallState::Finished(ToolStatus::Error) => ("✗", Style::new().fg(FAILURE)),
        CallState::Finished(ToolStatus::Denied) => ("⊘", Style::new().fg(DENIED)),
        CallState::Finished(ToolStatus::Cancelled) => ("–", Style::new().fg(THINKING)),
    }
}

fn tool_lines(tool: &ToolCell, width: usize) -> Vec<Line<'static>> {
    let (marker, style) = marker(tool.state);
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
    // Only while it is a question, plus the one answer the marker cannot
    // spell: ✓ and ⊘ already say allowed and denied, and a key list under a
    // prompt nobody can still answer is an instruction that does nothing.
    match prompt.answer {
        None => lines.push(Line::styled(
            "    [y] allow  [a] always allow  [n] deny".to_string(),
            style,
        )),
        Some(PermissionAnswer::AllowAlways) => {
            lines.push(Line::styled("    always allowed".to_string(), style));
        }
        Some(_) => {}
    }
    lines
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
