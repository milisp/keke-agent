//! The rewind overlay, docked where the picker is.
//!
//! Two steps, because a rewind is two questions: which prompt to go back to,
//! and what to put back when it does. The first is a list of what the person
//! said, newest first, so going back one turn — the common case by far — is
//! Esc Esc Enter Enter and nothing else.

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
use crate::rewind::Phase;
use crate::rewind::Rewind;

/// At most this many prompts are on screen at once, before the height cap.
const MAX_ROWS: usize = 10;
/// Two borders. A panel shorter than this plus one row cannot be read.
const CHROME: u16 = 2;
/// The confirm step's three choices, plus the line saying what is being wound
/// back and the one saying what a restore would touch.
const CONFIRM_ROWS: usize = 5;
/// How narrow the panel may be squeezed before it is not worth drawing.
const MIN_WIDTH: u16 = 24;

/// How many list rows this frame wants, before the cap.
fn wanted(rewind: &Rewind) -> usize {
    match rewind.phase() {
        Phase::Loading => 1,
        Phase::Picking { .. } => rewind.points().len().clamp(1, MAX_ROWS),
        Phase::Confirming { .. } => CONFIRM_ROWS,
    }
}

/// How tall the overlay is this frame. Zero when it is closed, or when the
/// height cap would leave it unreadable.
pub(crate) fn rows(app: &App, height: u16) -> u16 {
    let Some(rewind) = app.rewind() else {
        return 0;
    };
    let capped = (u16::try_from(wanted(rewind)).unwrap_or(1) + CHROME).min(height / 2);
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

fn dim() -> Style {
    Style::new().fg(Color::DarkGray)
}

fn highlight(selected: bool) -> Style {
    if selected {
        Style::new()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    }
}

/// The list of prompts.
fn picking<'a>(rewind: &'a Rewind, visible: usize) -> Vec<Line<'a>> {
    let selected = rewind.selected();
    // Scroll only far enough to keep the highlight on screen: the list opens
    // at the newest prompt and most people never leave it.
    let first = selected.saturating_sub(visible.saturating_sub(1));
    rewind
        .points()
        .iter()
        .enumerate()
        .skip(first)
        .take(visible)
        .map(|(at, point)| {
            let mut spans = vec![Span::styled(
                label(point.turn, &point.text),
                highlight(at == selected),
            )];
            // A turn that wrote is worth marking on the list itself: it is the
            // one a person looking to undo an edit is hunting for.
            if point.has_snapshot {
                spans.push(Span::styled(
                    " ·  changed files",
                    if at == selected {
                        highlight(true)
                    } else {
                        dim()
                    },
                ));
            }
            Line::from(spans)
        })
        .collect()
}

/// The three things a rewind can put back.
fn confirming<'a>(rewind: &'a Rewind) -> Vec<Line<'a>> {
    let selected = rewind.selected();
    let mut lines = Vec::with_capacity(CONFIRM_ROWS);
    if let Some(point) = rewind.point() {
        lines.push(Line::styled(
            label(point.turn, &point.text),
            Style::new().add_modifier(Modifier::BOLD),
        ));
    }
    lines.push(match rewind.changed() {
        None => Line::styled("  checking what changed\u{2026}", dim()),
        Some([]) => Line::styled("  no file changes since this turn", dim()),
        Some(files) => Line::styled(
            format!(
                "  {} file{} changed since: {}",
                files.len(),
                if files.len() == 1 { "" } else { "s" },
                files
                    .iter()
                    .take(3)
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            dim(),
        ),
    });
    for (at, choice) in rewind.choices().into_iter().enumerate() {
        let mut spans = vec![Span::styled(
            format!(" {} ", choice.label),
            match choice.unavailable {
                Some(_) => dim(),
                None => highlight(at == selected),
            },
        )];
        if let Some(reason) = choice.unavailable {
            spans.push(Span::styled(format!("\u{2014} {reason}"), dim()));
        }
        lines.push(Line::from(spans));
    }
    lines
}

pub(crate) fn draw(frame: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 || area.width < MIN_WIDTH {
        return;
    }
    let Some(rewind) = app.rewind() else {
        return;
    };
    let visible = usize::from(area.height.saturating_sub(CHROME)).max(1);
    let (title, lines) = match rewind.phase() {
        Phase::Loading => (
            " go back to ",
            vec![Line::styled("  looking\u{2026}", dim())],
        ),
        Phase::Picking { .. } => (
            " go back to \u{2014} enter chooses, esc cancels ",
            picking(rewind, visible),
        ),
        Phase::Confirming { .. } => (
            " put back \u{2014} enter confirms, esc goes back ",
            confirming(rewind),
        ),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::Cyan))
        .title(title);
    frame.render_widget(Paragraph::new(lines).block(block), area);
}
