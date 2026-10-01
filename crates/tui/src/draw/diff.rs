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
use unicode_width::UnicodeWidthStr;

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
    /// fight with it again. Failing that, [`terminal_light::background_color`]
    /// queries the terminal's own reported background over the `OSC 11`
    /// escape sequence (falling back to `COLORFGBG` where that query goes
    /// unanswered). Unlike a
    /// name- or env-based guess it reads the actual colour, so it gets
    /// terminals like macOS Terminal.app right without special-casing them.
    pub(crate) fn detect() -> Theme {
        static THEME: OnceLock<Theme> = OnceLock::new();
        *THEME.get_or_init(|| {
            env_override().unwrap_or_else(|| {
                match terminal_background() {
                    // 0.6 is terminal-light's own suggested pivot between a
                    // "rather dark" and "rather light" background.
                    Some((_, luma)) if luma > 0.6 => Theme::Light,
                    _ => Theme::Dark,
                }
            })
        })
    }

    /// Background tint for a user message in the transcript.
    pub(crate) fn user_bg(self) -> Color {
        let ((r, g, b), tint, alpha) = match self {
            Theme::Dark => (
                terminal_background().map_or((0, 0, 0), |(rgb, _)| rgb),
                255.0,
                0.12,
            ),
            Theme::Light => (
                terminal_background().map_or((255, 255, 255), |(rgb, _)| rgb),
                0.0,
                0.04,
            ),
        };
        let blend = |channel: u8| (f32::from(channel) * (1.0 - alpha) + tint * alpha) as u8;
        Color::Rgb(blend(r), blend(g), blend(b))
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

fn terminal_background() -> Option<((u8, u8, u8), f32)> {
    static BACKGROUND: OnceLock<Option<((u8, u8, u8), f32)>> = OnceLock::new();
    *BACKGROUND.get_or_init(|| {
        terminal_light::background_color().ok().map(|color| {
            let rgb = color.rgb();
            ((rgb.r, rgb.g, rgb.b), color.luma())
        })
    })
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

/// Render persisted diff rows, keeping source separate from display gutters.
pub(crate) fn push_diff_block(
    lines: &mut Vec<Line<'static>>,
    selectable: &mut Vec<Option<(usize, usize)>>,
    prefix: &str,
    hunk: &str,
    width: usize,
) {
    let theme = Theme::detect();
    // A narrow viewport must still leave room for source, not just its gutter.
    let prefix_width = prefix.width().min(width.saturating_sub(2));
    let indent = " ".repeat(prefix_width);
    let body = width.saturating_sub(prefix_width).max(1);
    let gutter_width = diff_gutter_width(hunk);
    // Headers and file headings already in `lines` have no source-only range.
    selectable.resize(lines.len(), None);
    let mut first = true;
    for line in hunk.split('\n') {
        let gap = line.trim() == "⋮";
        let parsed = gutter_width
            .and_then(|width| diff_columns(line, width))
            .or_else(|| legacy_columns(line));
        let (gutter, code, marker) = match parsed {
            Some(row) => row,
            None if gap => (String::new(), "⋮", ' '),
            None => (String::new(), line, line.chars().next().unwrap_or(' ')),
        };
        let style = diff_line_style(marker, theme);
        let gutter = if gutter.width() + 2 <= body {
            gutter
        } else {
            String::new()
        };
        let columns = gutter.width();
        let code_width = body.saturating_sub(columns).max(1);
        let chunks = wrap_code(code, code_width);
        for (index, chunk) in chunks.into_iter().enumerate() {
            let chunk_chars = chunk.chars().count();
            let lead = if first && prefix.width() == prefix_width {
                prefix
            } else {
                indent.as_str()
            };
            // A tinted background reads as a change only if it runs the full
            // row — GitHub fills the line, not just the text — so an added or
            // removed line is padded out to `body` before it is styled.
            let content = if style.bg.is_some() {
                format!(
                    "{chunk}{}",
                    " ".repeat(code_width.saturating_sub(chunk.width()))
                )
            } else {
                chunk
            };
            let from = lead.chars().count() + columns;
            selectable.push(Some((from, from + if gap { 0 } else { chunk_chars })));
            let displayed_gutter = if index == 0 {
                gutter.clone()
            } else {
                " ".repeat(columns)
            };
            lines.push(Line::from(vec![
                Span::raw(lead.to_string()),
                Span::styled(displayed_gutter, Style::new().fg(THINKING)),
                Span::styled(content, style),
            ]));
            first = false;
        }
    }
}

/// Validate the complete ASCII gutter before slicing: old rollouts, fallback
/// summaries and truncation notices are text, not necessarily two-column rows.
fn diff_columns(line: &str, width: usize) -> Option<(String, &str, char)> {
    let bytes = line.as_bytes();
    let first_end = 1 + width;
    let second_start = first_end + 1;
    let second_end = second_start + 1 + width;
    let old = line.get(1..first_end)?;
    let new = line.get(second_start + 1..second_end)?;
    let number = |cell: &str| {
        let digits = cell.trim_start_matches(' ');
        !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
    };
    let marker = match (*bytes.first()?, *bytes.get(second_start)?) {
        (b'-', b' ') if number(old) && new.bytes().all(|b| b == b' ') => '-',
        (b' ', b'+') if old.bytes().all(|b| b == b' ') && number(new) => '+',
        (b' ', b' ') if number(old) && number(new) => ' ',
        _ => return None,
    };
    if bytes.get(first_end) != Some(&b' ') || line.get(second_end..second_end + 2)? != "  " {
        return None;
    }
    Some((
        format!(
            "{old}{} {new}{}  ",
            char::from(bytes[0]),
            char::from(bytes[second_start])
        ),
        line.get(second_end + 2..)?,
        marker,
    ))
}

fn diff_gutter_width(hunk: &str) -> Option<usize> {
    hunk.lines().filter_map(diff_row_width).find(|&width| {
        // An indented legacy deletion can look like a two-column row.
        // Choose a format for the entire hunk, or that row's number and
        // marker gain a padding cell while its neighbours stay put.
        hunk.lines().all(|line| {
            (legacy_columns(line).is_none() && diff_row_width(line).is_none())
                || diff_columns(line, width).is_some()
        })
    })
}

fn diff_row_width(line: &str) -> Option<usize> {
    let ascii_prefix = line
        .bytes()
        .take_while(|b| matches!(b, b' ' | b'-' | b'+') || b.is_ascii_digit())
        .count();
    (1..=ascii_prefix / 2).find(|&width| diff_columns(line, width).is_some())
}

/// Rollouts predating the two-column format used `<marker> <number> <code>`.
/// Consume exactly one separator after the number, never source indentation.
fn legacy_columns(line: &str) -> Option<(String, &str, char)> {
    let bytes = line.as_bytes();
    let marker = *bytes.first()?;
    if !matches!(marker, b' ' | b'-' | b'+') || bytes.get(1) != Some(&b' ') {
        return None;
    }
    let mut cursor = 2;
    while bytes.get(cursor) == Some(&b' ') {
        cursor += 1;
    }
    let start = cursor;
    while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
        cursor += 1;
    }
    if cursor == start || bytes.get(cursor) != Some(&b' ') {
        return None;
    }
    Some((
        format!("{}{} ", line.get(2..cursor)?, char::from(marker)),
        line.get(cursor + 1..)?,
        char::from(marker),
    ))
}

// Source code must retain indentation and wrap by terminal cells, not words.
fn wrap_code(text: &str, width: usize) -> Vec<String> {
    let text = text.replace('\t', "    ");
    let span = Span::raw(text);
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut used = 0;
    for grapheme in span.styled_graphemes(Style::default()) {
        let count = grapheme.symbol.width();
        if used > 0 && used + count > width {
            rows.push(std::mem::take(&mut row));
            used = 0;
        }
        row.push_str(grapheme.symbol);
        used += count;
    }
    rows.push(row);
    rows
}

/// Tint changed source without giving context rows a change background.
fn diff_line_style(marker: char, theme: Theme) -> Style {
    match marker {
        '+' => Style::new().fg(SUCCESS).bg(theme.add_bg()),
        '-' => Style::new().fg(FAILURE).bg(theme.del_bg()),
        _ => Style::new().fg(THINKING),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gutters_include_rows_with_only_one_line_number() {
        assert_eq!(diff_gutter_width("-12      old\n    +13  new"), Some(2));
        assert_eq!(diff_columns(" 12  13  same", 2).unwrap().1, "same");
    }

    #[test]
    fn code_preserves_spaces_and_wraps_by_display_width() {
        assert_eq!(wrap_code("    a  b", 6), vec!["    a ", " b"]);
        assert_eq!(wrap_code("中文ab", 4), vec!["中文", "ab"]);
        assert_eq!(wrap_code("\tcode", 20), vec!["    code"]);
        assert_eq!(wrap_code("👩‍💻e\u{301}ab", 3), vec!["👩‍💻e\u{301}", "ab"]);
    }

    fn source_rows(hunk: &str, width: usize) -> (Vec<Line<'static>>, Vec<String>) {
        let mut lines = Vec::new();
        let mut ranges = Vec::new();
        push_diff_block(&mut lines, &mut ranges, "      ", hunk, width);
        let source = lines
            .iter()
            .zip(ranges)
            .map(|(line, range)| {
                let (from, to) = range.unwrap();
                line.to_string()
                    .chars()
                    .skip(from)
                    .take(to - from)
                    .collect()
            })
            .collect();
        (lines, source)
    }

    #[test]
    fn old_rollouts_keep_their_source_and_change_styles() {
        let (lines, source) = source_rows("-  12     old_value\n+  12 中文new_value\n   13 42", 80);
        assert_eq!(source, ["    old_value", "中文new_value", "42"]);
        assert!(lines[0].spans.last().unwrap().style.bg.is_some());
        assert!(lines[1].spans.last().unwrap().style.bg.is_some());
        assert!(lines[2].spans.last().unwrap().style.bg.is_none());
    }

    #[test]
    fn indented_legacy_deletions_keep_numbers_and_markers_in_the_same_column() {
        let hunk = "- 12         indented\n- 13 plain\n+ 12 replacement";
        assert_eq!(diff_gutter_width(hunk), None);
        let (lines, source) = source_rows(hunk, 80);
        assert_eq!(source, ["        indented", "plain", "replacement"]);
        assert_eq!(lines[0].spans[1].content, "12- ");
        assert_eq!(lines[1].spans[1].content, "13- ");
        assert_eq!(lines[2].spans[1].content, "12+ ");
        for line in lines {
            let text = line.to_string();
            assert!(matches!(text.as_bytes()[8], b'-' | b'+'));
        }
    }

    #[test]
    fn padded_two_column_deletions_do_not_fall_back_to_legacy_gutters() {
        let hunk = format!(
            "-{:>3} {:>4}  {}\n-{:>3} {:>4}  {}\n{:>4} +{:>3}  {}",
            12, "", "  indented", 123, "", "plain", "", 124, "replacement",
        );
        assert_eq!(diff_gutter_width(&hunk), Some(3));
        let (lines, source) = source_rows(&hunk, 80);
        assert_eq!(source, ["  indented", "plain", "replacement"]);
        assert_eq!(lines[0].spans[1].content, " 12-       ");
        assert_eq!(lines[1].spans[1].content, "123-       ");
    }

    #[test]
    fn arbitrary_unicode_and_truncation_notices_are_never_sliced_as_gutters() {
        for text in [
            "+中文中文",
            "-é中a",
            "… diff truncated",
            "edited 中文.rs",
            "- 2",
            " 12  13",
        ] {
            let (_, source) = source_rows(text, 80);
            assert_eq!(source, [text]);
        }
        let (_, source) = source_rows("   +1  中文\n… diff truncated", 80);
        assert_eq!(source, ["中文", "… diff truncated"]);
    }

    #[test]
    fn numeric_source_is_not_a_line_number_and_empty_code_is_valid() {
        let (_, source) = source_rows("-12      42\n    +13      99\n 14  15  ", 80);
        assert_eq!(source, ["42", "    99", ""]);
    }

    #[test]
    fn narrow_viewports_drop_gutters_instead_of_hiding_source() {
        for width in [2, 8, 12, 16, 20] {
            let (lines, source) = source_rows("    +13  中文abcdef", width);
            assert_eq!(source.concat(), "中文abcdef");
            assert!(
                lines.iter().all(|line| line.width() <= width),
                "{width}: {lines:?}"
            );
        }
    }

    #[test]
    fn copy_ranges_follow_existing_headers_and_exclude_hunk_gaps() {
        let mut lines = vec![Line::raw("header"), Line::raw("file.rs")];
        let mut ranges = Vec::new();
        push_diff_block(
            &mut lines,
            &mut ranges,
            "",
            "   +1  code\n   ⋮\n   +9  next",
            80,
        );
        assert_eq!(ranges.len(), lines.len());
        assert_eq!(&ranges[..2], &[None, None]);
        let (from, to) = ranges[3].unwrap();
        assert_eq!(from, to, "a gap marker is not source");
        let (from, to) = ranges[4].unwrap();
        assert_eq!(
            lines[4]
                .to_string()
                .chars()
                .skip(from)
                .take(to - from)
                .collect::<String>(),
            "next"
        );
    }

    #[test]
    fn markers_move_after_numbers_without_widening_the_gutter() {
        let mut lines = Vec::new();
        let mut ranges = Vec::new();
        push_diff_block(
            &mut lines,
            &mut ranges,
            "",
            "-12      old\n    +13  new",
            20,
        );
        assert_eq!(lines[0].spans[1].content, "12-      ");
        assert_eq!(lines[1].spans[1].content, "    13+  ");
        assert_eq!(ranges[0], Some((9, 12)));
    }
}
