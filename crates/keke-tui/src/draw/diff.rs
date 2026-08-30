//! Diff-hunk rendering: colouring an edit/write tool's diff block the way
//! GitHub does — added/removed lines carry the change, so they are the one
//! place in the transcript that earns per-line colour instead of one style
//! for the whole block.

use std::sync::OnceLock;

use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use super::transcript::FAILURE;
use super::transcript::SUCCESS;
use super::transcript::THINKING;
use super::transcript::wrap;

/// Whether the terminal's own background is dark or light.
///
/// Ratatui never tells a widget what the terminal looks like, so this is a
/// best-effort guess — see [`Theme::detect`] — that falls back to dark when
/// nothing can tell it otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Theme {
    Dark,
    Light,
}

impl Theme {
    /// Detects the theme once per process.
    ///
    /// `KEKE_THEME` wins outright, since detection is a heuristic and a
    /// person who has already fought with it once should never have to
    /// fight with it again. Failing that, [`terminal_light::luma`] queries
    /// the terminal's own reported background over the `OSC 11` escape
    /// sequence (falling back to `COLORFGBG` where that query goes
    /// unanswered) — this is the same probe codex's TUI uses, and unlike a
    /// name- or env-based guess it reads the actual colour, so it gets
    /// terminals like macOS Terminal.app right without special-casing them.
    pub(crate) fn detect() -> Theme {
        static THEME: OnceLock<Theme> = OnceLock::new();
        *THEME.get_or_init(|| {
            env_override().unwrap_or_else(|| {
                match terminal_light::luma() {
                    // 0.6 is terminal-light's own suggested pivot between a
                    // "rather dark" and "rather light" background.
                    Ok(luma) if luma > 0.6 => Theme::Light,
                    _ => Theme::Dark,
                }
            })
        })
    }

    /// Background tint for an added line.
    fn add_bg(self) -> Color {
        match self {
            // Dark tints, not GitHub's pastel add/remove backgrounds — a
            // light tint picked for a dark terminal would wash the text out.
            Theme::Dark => Color::Rgb(20, 46, 26),
            // GitHub's own light-mode pastels — a dark tint here would read
            // as a solid block on a light background instead of a tint.
            Theme::Light => Color::Rgb(230, 255, 236),
        }
    }

    /// Background tint for a removed line.
    fn del_bg(self) -> Color {
        match self {
            Theme::Dark => Color::Rgb(56, 24, 24),
            Theme::Light => Color::Rgb(255, 235, 233),
        }
    }
}

/// Explicit override for when detection guesses wrong or a terminal cannot
/// be probed at all.
fn env_override() -> Option<Theme> {
    match std::env::var("KEKE_THEME")
        .ok()?
        .to_ascii_lowercase()
        .as_str()
    {
        "light" => Some(Theme::Light),
        "dark" => Some(Theme::Dark),
        _ => None,
    }
}

/// Like `push_block`, but colours each line of a unified diff.
pub(crate) fn push_diff_block(
    lines: &mut Vec<Line<'static>>,
    prefix: &str,
    hunk: &str,
    width: usize,
) {
    let theme = Theme::detect();
    let indent = " ".repeat(prefix.chars().count());
    let body = width.saturating_sub(prefix.chars().count()).max(1);
    let mut first = true;
    for line in hunk.split('\n') {
        let style = diff_line_style(line, theme);
        for chunk in wrap(line, body) {
            let lead = if first { prefix } else { indent.as_str() };
            // A tinted background reads as a change only if it runs the full
            // row — GitHub fills the line, not just the text — so an added or
            // removed line is padded out to `body` before it is styled.
            let content = if style.bg.is_some() {
                format!("{chunk:<body$}")
            } else {
                chunk
            };
            lines.push(Line::from(vec![
                Span::styled(lead.to_string(), style),
                Span::styled(content, style),
            ]));
            first = false;
        }
    }
}

/// Colour for one line of an edit's diff hunk — a `<marker> <line number>
/// <text>` row, not a unified-diff patch, so this reads the leading marker
/// rather than hunting for `+++`/`@@`.
///
/// GitHub's diff view marks a changed line by tinting its whole row, not by
/// colouring the text — a wall of green/red text is what a terminal `diff`
/// looks like, and that's the effect this was asked to move away from.
fn diff_line_style(line: &str, theme: Theme) -> Style {
    match line.as_bytes().first() {
        Some(b'+') => Style::new().fg(SUCCESS).bg(theme.add_bg()),
        Some(b'-') => Style::new().fg(FAILURE).bg(theme.del_bg()),
        _ => Style::new().fg(THINKING),
    }
}
