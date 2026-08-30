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

use super::diff::push_diff_block;
use super::markdown;
use crate::transcript::CallState;
use crate::transcript::Cell;
use crate::transcript::PermissionCell;
use crate::transcript::ToolCell;
use crate::transcript::verb;

const USER: Color = Color::Cyan;
pub(super) const THINKING: Color = Color::DarkGray;
const NOTICE: Color = Color::Blue;
pub(super) const FAILURE: Color = Color::Red;
const DENIED: Color = Color::Yellow;
pub(super) const SUCCESS: Color = Color::Green;

/// A drawn transcript, plus where its expandable headers landed.
///
/// A click arrives as a screen position and nothing else, so the frame that
/// drew a header is the only thing that can say which cell it belongs to.
#[derive(Debug, Default)]
pub(crate) struct Rendered {
    pub lines: Vec<Line<'static>>,
    /// `(line index, cell key)` for every row a click may expand or collapse.
    pub toggles: Vec<(usize, usize)>,
    /// `(plan line, line index)` for the last plan drawn, so the frame can
    /// scroll a selection that lives inside the scrollback into view.
    pub plan_lines: Vec<(usize, usize)>,
}

/// Width of the plan's line-number gutter, including its trailing space.
const PLAN_GUTTER: usize = 5;

/// The plan, numbered line by line.
///
/// Numbered rather than rendered as markdown because a comment says "line 7",
/// so line 7 has to be something a person can see and point at. A line too
/// long for the screen wraps, and every row it wraps onto still belongs to it.
fn plan_lines(out: &mut Rendered, cell: &crate::transcript::PlanCell, width: usize) {
    let header = match cell.answer {
        None => Span::styled(
            " plan ",
            Style::new()
                .fg(Color::Black)
                .bg(DENIED)
                .add_modifier(Modifier::BOLD),
        ),
        Some(PermissionAnswer::Deny) => {
            Span::styled(" plan · sent back ", Style::new().fg(THINKING))
        }
        Some(_) => Span::styled(" plan · approved ", Style::new().fg(SUCCESS)),
    };
    out.lines.push(Line::from(header));

    if cell.text.trim().is_empty() {
        // An agent that left plan mode without writing anything is not an
        // error and not a blank space: a person can still approve and get on
        // with it, so the cell says what happened.
        out.lines.push(Line::styled(
            "  the agent proposed no plan",
            Style::new().fg(DENIED),
        ));
    }
    let body = width.saturating_sub(PLAN_GUTTER).max(8);
    for (index, line) in cell.text.lines().enumerate() {
        for part in markdown::wrap_plain(line, body) {
            out.plan_lines.push((index, out.lines.len()));
            out.lines.push(Line::from(vec![
                Span::styled(format!("{:>4} ", index + 1), Style::new().fg(THINKING)),
                Span::raw(part),
            ]));
        }
    }
    // Where it was saved, so a person can open it, edit it, or send it to
    // somebody without asking the agent to repeat itself.
    if let Some(path) = &cell.path {
        out.lines.push(Line::styled(
            format!("  {}", path.display()),
            Style::new().fg(THINKING),
        ));
    }
    out.lines.push(Line::raw(""));
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

pub(crate) fn render(cells: &[Cell], width: u16, expanded: &HashSet<usize>) -> Rendered {
    let width = usize::from(width.max(8));
    let mut out = Rendered::default();
    let mut index = 0;
    while index < cells.len() {
        let cell = &cells[index];
        match cell {
            Cell::Plan(plan_cell) => {
                plan_lines(&mut out, plan_cell, width);
            }
            Cell::User(text) => {
                push_block(&mut out.lines, "› ", text, Style::new().fg(USER), width);
            }
            // Prose that follows reasoning usually starts with the blank line
            // that separated the two on the wire. That separator is not part
            // of the answer, and drawn it is an empty row where the reasoning
            // used to be, so the block is trimmed to its own text.
            Cell::Assistant(text) => {
                let text = text.trim_matches('\n');
                if text.is_empty() {
                    index += 1;
                    continue;
                }
                out.lines
                    .extend(markdown::render(text, width, Style::new(), ""));
            }
            Cell::Tool(first) => {
                let run = verb(&first.name).0;
                let end = cells[index..]
                    .iter()
                    .position(|cell| !runs_with(cell, run))
                    .map_or(cells.len(), |offset| index + offset);
                out.toggles.push((out.lines.len(), index));
                let group = &cells[index..end];
                let open = default_open(group) ^ expanded.contains(&index);
                out.lines.extend(group_lines(group, open, width));
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
            // Drawn verbatim, one line each: it is fixed-layout art, not
            // prose, so wrapping it would misalign the icon against the text
            // beside it.
            Cell::Banner(lines) => {
                out.lines.extend(
                    lines
                        .iter()
                        .map(|line| Line::styled(line.clone(), Style::new().fg(THINKING))),
                );
            }
        }
        out.lines.push(Line::default());
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
    let has_running = tools
        .iter()
        .any(|tool| matches!(tool.state, CallState::Running));
    let status = tools
        .iter()
        .fold(ToolStatus::Ok, |worst_so_far, tool| match tool.state {
            CallState::Finished(status) => worst(status, worst_so_far),
            CallState::Running => worst_so_far,
        });
    // The spinner wins the header glyph while anything in the run is still
    // going, even if an earlier call in it already failed — a finished
    // failure keeps the group open (see `default_open`), but the run itself
    // is not done yet, and a red header before that is true reads as if it
    // were.
    let (marker, style) = if has_running {
        marker(CallState::Running)
    } else {
        marker(CallState::Finished(status))
    };
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
        // An edit/write's raw `old_string=... new_string=...` dump is exactly
        // what the diff below already shows, one line at a time instead of as
        // a wall of `key=value` text — showing both says the same thing twice.
        if tool.arguments != tool.summary && !crate::transcript::is_diff_tool(&tool.name) {
            push_block(
                &mut lines,
                "      ",
                &tool.arguments,
                Style::new().fg(THINKING),
                width,
            );
        }
        if let Some(detail) = &tool.detail {
            if crate::transcript::is_diff_tool(&tool.name) {
                push_diff_block(&mut lines, "      ", detail, width);
            } else {
                push_block(
                    &mut lines,
                    "      ",
                    detail,
                    Style::new().fg(THINKING),
                    width,
                );
            }
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

/// Whether a cell belongs in the run being gathered for drawing.
///
/// Unlike [`crate::transcript::groups_with`] — which only chains *finished*
/// calls, for the keyboard's "reopen the last run" gesture — this also folds
/// in a still-running call of the same kind, since two `read_file`s drawn
/// back to back read as one action in progress, not two.
fn runs_with(cell: &Cell, run: &str) -> bool {
    matches!(cell, Cell::Tool(tool) if verb(&tool.name).0 == run)
}

/// Whether a run should be open before any manual toggle is applied.
///
/// A run still in flight is shown as it happens rather than behind a fold a
/// person has to know to open. One that finished cleanly folds away, since by
/// then it is confirmed and re-reading it adds nothing; one that ended in an
/// error or a denial stays open, because that is exactly the case a person
/// needs to see without hunting for it.
fn default_open(group: &[Cell]) -> bool {
    group.iter().any(|cell| match cell {
        Cell::Tool(tool) => match tool.state {
            CallState::Running => true,
            CallState::Finished(ToolStatus::Error | ToolStatus::Denied) => true,
            CallState::Finished(ToolStatus::Ok) => {
                crate::transcript::is_diff_tool(&tool.name) && tool.detail.is_some()
            }
            CallState::Finished(ToolStatus::Cancelled) => false,
        },
        _ => false,
    })
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
pub(super) fn wrap(text: &str, width: usize) -> Vec<String> {
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
