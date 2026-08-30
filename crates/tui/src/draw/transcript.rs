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
                let end = cells[index..]
                    .iter()
                    .position(|cell| !runs_with(cell, &first.name))
                    .map_or(cells.len(), |offset| index + offset);
                out.toggles.push((out.lines.len(), index));
                let group = &cells[index..end];
                let open = default_open(group) ^ expanded.contains(&index);
                out.lines.extend(group_lines(group, open, width));
                index = end;
                out.lines.push(Line::default());
                continue;
            }
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

    // A run of one, or several calls to the same tool, keeps naming that
    // tool directly (`Read 3 files`) — the noun already says what happened.
    // Only a run that mixes exploration tools (`read_file`, `list_dir`,
    // `grep`) needs the umbrella name, since there is no single noun for
    // "read, then listed, then read again".
    let same_name = tools.iter().all(|tool| tool.name == first.name);
    let (title, summary) = if same_name {
        let (verb, noun) = verb(&first.name);
        let summary = if tools.len() == 1 {
            first.summary.clone()
        } else {
            format!("{} {noun}", tools.len())
        };
        (verb, summary)
    } else {
        let title = if has_running { "Exploring" } else { "Explored" };
        (title, format!("{} steps", tools.len()))
    };

    let mut lines = Vec::new();
    lines.push(header(marker, title, &summary, open, style));
    if !open {
        return lines;
    }
    if tools.len() > 1 && !same_name {
        // Consecutive reads or lists inside a mixed run collapse into one
        // line, named once, since a reader wants "Read a.rs, b.rs" or
        // "Listed src/, tests/" rather than the same verb repeated for every
        // file or folder.
        let mut index = 0;
        while index < tools.len() {
            let tool = tools[index];
            if tool.name == "read_file" || tool.name == "list_dir" {
                let (verb, _) = verb(&tool.name);
                let mut names = Vec::new();
                let mut states = Vec::new();
                let mut end = index;
                while end < tools.len() && tools[end].name == tool.name {
                    let name = basename(&tools[end].summary);
                    if !names.contains(&name) {
                        names.push(name);
                    }
                    states.push(tools[end].state);
                    end += 1;
                }
                let (glyph, style) = run_marker(&states);
                lines.push(Line::from(vec![
                    Span::styled(format!("    {glyph} "), style),
                    Span::styled(format!("{verb} "), Style::new().fg(THINKING)),
                    Span::styled(names.join(", "), Style::new().fg(THINKING)),
                ]));
                index = end;
            } else {
                let (glyph, style) = self::marker(tool.state);
                let (verb, _) = verb(&tool.name);
                lines.push(Line::from(vec![
                    Span::styled(format!("    {glyph} "), style),
                    Span::styled(format!("{verb} "), Style::new().fg(THINKING)),
                    Span::styled(tool.summary.clone(), Style::new().fg(THINKING)),
                ]));
                index += 1;
            }
        }
    } else if tools.len() > 1 {
        for tool in &tools {
            let (glyph, style) = self::marker(tool.state);
            lines.push(Line::from(vec![
                Span::styled(format!("    {glyph} "), style),
                Span::styled(tool.summary.clone(), Style::new().fg(THINKING)),
            ]));
        }
    }
    for tool in &tools {
        // An edit/write's raw `old_string=... new_string=...` dump is exactly
        // what the diff below already shows, one line at a time instead of as
        // a wall of `key=value` text — showing both says the same thing twice.
        // A read-only exploration call's headline already names the one
        // thing that mattered (`path=...`, `pattern=...`); the raw
        // `key=value` dump underneath it would just repeat that.
        if tool.arguments != tool.summary
            && !crate::transcript::is_diff_tool(&tool.name)
            && !crate::transcript::is_exploration_tool(&tool.name)
        {
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

/// The last path segment, so a mixed exploration run reads "Listed demo,
/// src" rather than repeating the full path for every entry — the run's
/// header already gives the count, and the surrounding path is exactly what
/// the reader has to skip past to see what actually changed.
fn basename(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|part| !part.is_empty())
        .unwrap_or(path)
        .to_string()
}

/// The glyph and colour for a coalesced run of calls: running wins outright,
/// otherwise the worst finished status does — same rule as [`worst`], just
/// applied to a handful of states gathered ad hoc instead of a whole group.
fn run_marker(states: &[CallState]) -> (&'static str, Style) {
    let has_running = states
        .iter()
        .any(|state| matches!(state, CallState::Running));
    if has_running {
        return marker(CallState::Running);
    }
    let status = states
        .iter()
        .fold(ToolStatus::Ok, |worst_so_far, state| match state {
            CallState::Finished(status) => worst(*status, worst_so_far),
            CallState::Running => worst_so_far,
        });
    marker(CallState::Finished(status))
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
fn runs_with(cell: &Cell, anchor: &str) -> bool {
    matches!(cell, Cell::Tool(tool) if crate::transcript::same_run(&tool.name, anchor))
}

/// Whether a run should be open before any manual toggle is applied.
///
/// A run still in flight is shown as it happens rather than behind a fold a
/// person has to know to open. One that finished cleanly folds away, since by
/// then it is confirmed and re-reading it adds nothing; one that ended in an
/// error or a denial stays open, because that is exactly the case a person
/// needs to see without hunting for it. An exploration run (`read_file`,
/// `list_dir`, `grep`) always stays open, clean or not: it names what it
/// looked at, not what it changed, so folding it away on success would hide
/// the one thing worth knowing — what the agent actually read — behind a
/// click nobody has reason to make.
fn default_open(group: &[Cell]) -> bool {
    group.iter().any(|cell| match cell {
        Cell::Tool(tool) => match tool.state {
            CallState::Running => true,
            CallState::Finished(ToolStatus::Error | ToolStatus::Denied) => true,
            CallState::Finished(ToolStatus::Ok) => {
                crate::transcript::is_exploration_tool(&tool.name)
                    || (crate::transcript::is_diff_tool(&tool.name) && tool.detail.is_some())
            }
            CallState::Finished(ToolStatus::Cancelled) => false,
        },
        _ => false,
    })
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

#[cfg(test)]
mod grouping_tests {
    use keke_protocol::ToolCallId;

    use super::*;
    use crate::transcript::ToolCell;

    fn tool(id: &str, name: &str, summary: &str) -> Cell {
        Cell::Tool(ToolCell {
            id: ToolCallId::new(id),
            name: name.to_string(),
            summary: summary.to_string(),
            arguments: summary.to_string(),
            state: CallState::Finished(ToolStatus::Ok),
            detail: None,
        })
    }

    fn header_titles(rendered: &Rendered) -> Vec<String> {
        rendered
            .toggles
            .iter()
            .map(|(line, _)| {
                rendered.lines[*line]
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn mixed_exploration_run_becomes_one_explored_group() {
        let cells = vec![
            tool("c1", "read_file", "a.rs"),
            tool("c2", "list_dir", "src/"),
            tool("c3", "read_file", "b.rs"),
        ];
        let rendered = render(&cells, 80, &HashSet::new());
        assert_eq!(rendered.toggles.len(), 1, "one collapsed run, not three");
        let titles = header_titles(&rendered);
        assert!(titles[0].contains("Explored"), "got {titles:?}");
        assert!(titles[0].contains("3 steps"), "got {titles:?}");
    }

    #[test]
    fn a_single_read_still_names_itself() {
        let cells = vec![tool("c1", "read_file", "a.rs")];
        let rendered = render(&cells, 80, &HashSet::new());
        let titles = header_titles(&rendered);
        assert!(titles[0].contains("Read"), "got {titles:?}");
        assert!(titles[0].contains("a.rs"), "got {titles:?}");
    }

    #[test]
    fn a_run_of_the_same_tool_keeps_its_own_verb() {
        let cells = vec![
            tool("c1", "read_file", "a.rs"),
            tool("c2", "read_file", "b.rs"),
        ];
        let rendered = render(&cells, 80, &HashSet::new());
        assert_eq!(rendered.toggles.len(), 1);
        let titles = header_titles(&rendered);
        assert!(titles[0].contains("Read"), "got {titles:?}");
        assert!(titles[0].contains("2 files"), "got {titles:?}");
    }

    #[test]
    fn mixed_exploration_run_lists_read_files_and_listed_folders() {
        let cells = vec![
            tool("c1", "read_file", "/repo/demo/a.rs"),
            tool("c2", "list_dir", "/repo/demo"),
            tool("c3", "list_dir", "/repo/demo/src"),
            tool("c4", "read_file", "/repo/demo/b.rs"),
        ];
        let rendered = render(&cells, 80, &HashSet::new());
        let body: String = rendered.lines[1..4]
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect();
        assert!(body.contains("Read a.rs"), "got {body:?}");
        assert!(body.contains("Listed demo, src"), "got {body:?}");
    }

    #[test]
    fn bash_does_not_join_an_exploration_run() {
        let cells = vec![tool("c1", "read_file", "a.rs"), tool("c2", "bash", "ls")];
        let rendered = render(&cells, 80, &HashSet::new());
        assert_eq!(rendered.toggles.len(), 2, "bash stays its own group");
    }
}
