//! Rendering agent prose (`Cell::Assistant`, `Cell::Thinking`) as styled lines.
//!
//! grok-build carries its own multi-thousand-line markdown/mermaid/LaTeX
//! renderer for this; keke's transcript only needs the everyday subset a model
//! actually emits — headings, emphasis, code, lists, quotes — so this stays a
//! thin `pulldown-cmark` walk rather than a port. Malformed or plain-text input
//! renders as prose either way; `pulldown-cmark` never errors.

use pulldown_cmark::Alignment;
use pulldown_cmark::Event;
use pulldown_cmark::HeadingLevel;
use pulldown_cmark::Options;
use pulldown_cmark::Parser;
use pulldown_cmark::Tag;
use pulldown_cmark::TagEnd;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

const HEADING: Color = Color::Magenta;
const CODE: Color = Color::Yellow;
const RULE: Color = Color::DarkGray;

/// Render `text` as markdown, wrapped to `width`.
///
/// `base` is the style prose inherits (so a thought stays dim-italic); `lead`
/// is the indent already claimed by the caller's header, e.g. `"  "` for a
/// thinking block — matching `push_block`'s prefix/indent convention so the
/// two stay visually aligned.
pub(crate) fn render(text: &str, width: usize, base: Style, lead: &str) -> Vec<Line<'static>> {
    let options =
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS | Options::ENABLE_TABLES;
    let mut lines = Vec::new();
    let mut words: Vec<(String, Style)> = Vec::new();
    let mut style_stack = vec![base];
    let mut list_stack: Vec<Option<u64>> = Vec::new();
    let mut prefix = lead.to_string();
    let mut in_code_block = false;
    let mut code_block = String::new();
    let mut link_destination = None;

    let flush = |lines: &mut Vec<Line<'static>>, words: &mut Vec<(String, Style)>, prefix: &str| {
        if !words.is_empty() {
            lines.extend(wrap_words(std::mem::take(words), width, prefix));
        }
    };

    let mut parser = Parser::new_ext(text, options);
    while let Some(event) = parser.next() {
        let style = *style_stack.last().unwrap_or(&base);
        match event {
            Event::Start(Tag::Table(alignments)) => {
                flush(&mut lines, &mut words, &prefix);
                lines.extend(render_table(
                    &mut parser,
                    &alignments,
                    width,
                    style,
                    &prefix,
                ));
                lines.push(Line::default());
            }
            Event::Start(Tag::Heading { level, .. }) => {
                flush(&mut lines, &mut words, &prefix);
                style_stack.push(Style::new().fg(HEADING).add_modifier(Modifier::BOLD));
                let marks = "#".repeat(heading_rank(level));
                words.push((marks, *style_stack.last().unwrap_or(&base)));
            }
            Event::End(TagEnd::Heading(_)) => {
                flush(&mut lines, &mut words, &prefix);
                style_stack.pop();
                lines.push(Line::default());
            }
            Event::End(TagEnd::Paragraph) => {
                flush(&mut lines, &mut words, &prefix);
                lines.push(Line::default());
            }
            Event::Start(Tag::Strong) => style_stack.push(style.add_modifier(Modifier::BOLD)),
            Event::Start(Tag::Link { dest_url, .. }) => {
                style_stack.push(style.fg(Color::Cyan).add_modifier(Modifier::UNDERLINED));
                // Keep the destination visible: mouse hit testing uses the drawn
                // text, and a label alone conceals where a click would lead.
                link_destination = Some(dest_url.to_string());
            }
            Event::End(TagEnd::Link) => {
                if let Some(url) = link_destination.take()
                    && words.last().is_none_or(|(word, _)| word != &url)
                {
                    words.push((format!("({url})"), style));
                }
                style_stack.pop();
            }
            Event::End(TagEnd::Strong) => {
                style_stack.pop();
            }
            Event::Start(Tag::Emphasis) => style_stack.push(style.add_modifier(Modifier::ITALIC)),
            Event::End(TagEnd::Emphasis) => {
                style_stack.pop();
            }
            Event::Start(Tag::Strikethrough) => {
                style_stack.push(style.add_modifier(Modifier::CROSSED_OUT));
            }
            Event::End(TagEnd::Strikethrough) => {
                style_stack.pop();
            }
            Event::Code(code) if !in_code_block => {
                for word in code.split_whitespace() {
                    words.push((word.to_string(), Style::new().fg(CODE)));
                }
            }
            Event::Start(Tag::CodeBlock(_)) => {
                flush(&mut lines, &mut words, &prefix);
                in_code_block = true;
                code_block.clear();
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code_block = false;
                let indent = format!("{prefix}  ");
                let body = width.saturating_sub(indent.chars().count()).max(1);
                for raw in code_block.trim_end_matches('\n').split('\n') {
                    for chunk in wrap_plain(raw, body) {
                        lines.push(Line::styled(
                            format!("{indent}{chunk}"),
                            Style::new().fg(CODE),
                        ));
                    }
                }
                lines.push(Line::default());
            }
            Event::Start(Tag::Item) => {
                flush(&mut lines, &mut words, &prefix);
                let marker = match list_stack.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        m
                    }
                    _ => "- ".to_string(),
                };
                prefix.push_str(&" ".repeat(marker.chars().count()));
                words.push((marker, style));
            }
            Event::End(TagEnd::Item) => {
                flush(&mut lines, &mut words, &prefix);
                let drop = match list_stack.last() {
                    Some(Some(_)) => 3,
                    _ => 2,
                };
                prefix.truncate(prefix.len().saturating_sub(drop));
            }
            Event::Start(Tag::List(start)) => list_stack.push(start),
            Event::End(TagEnd::List(_)) => {
                list_stack.pop();
                lines.push(Line::default());
            }
            Event::Start(Tag::BlockQuote(_)) => prefix.push_str("> "),
            Event::End(TagEnd::BlockQuote(_)) => {
                prefix.truncate(prefix.len().saturating_sub(2));
            }
            Event::Text(text) | Event::Code(text) if in_code_block => {
                code_block.push_str(&text);
            }
            Event::Text(text) => {
                for word in text.split_whitespace() {
                    words.push((word.to_string(), style));
                }
            }
            Event::SoftBreak => words.push((String::new(), style)),
            Event::HardBreak => flush(&mut lines, &mut words, &prefix),
            Event::Rule => {
                flush(&mut lines, &mut words, &prefix);
                lines.push(Line::styled(
                    "─".repeat(width.max(1)),
                    Style::new().fg(RULE),
                ));
                lines.push(Line::default());
            }
            _ => {}
        }
    }
    flush(&mut lines, &mut words, &prefix);
    while lines.last().is_some_and(|line| line.spans.is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push(Line::default());
    }
    lines
}

/// Buffer cells so every row uses the same display-width column sizes.
fn render_table(
    parser: &mut Parser<'_>,
    alignments: &[Alignment],
    width: usize,
    base: Style,
    prefix: &str,
) -> Vec<Line<'static>> {
    let mut rows: Vec<Vec<Vec<Span<'static>>>> = Vec::new();
    let mut row = Vec::new();
    let mut cell = Vec::new();
    let mut styles = vec![base];
    for event in parser.by_ref() {
        let style = *styles.last().unwrap_or(&base);
        match event {
            Event::End(TagEnd::Table) => break,
            Event::Start(Tag::TableHead) => styles.push(base.add_modifier(Modifier::BOLD)),
            Event::End(TagEnd::TableHead) => {
                rows.push(std::mem::take(&mut row));
                styles.pop();
            }
            Event::End(TagEnd::TableRow) => rows.push(std::mem::take(&mut row)),
            Event::End(TagEnd::TableCell) => row.push(std::mem::take(&mut cell)),
            Event::Start(Tag::Strong) => styles.push(style.add_modifier(Modifier::BOLD)),
            Event::Start(Tag::Emphasis) => styles.push(style.add_modifier(Modifier::ITALIC)),
            Event::Start(Tag::Strikethrough) => {
                styles.push(style.add_modifier(Modifier::CROSSED_OUT));
            }
            Event::End(TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough) => {
                styles.pop();
            }
            Event::Text(text) => cell.push(Span::styled(text.into_string(), style)),
            Event::Code(text) => cell.push(Span::styled(text.into_string(), style.fg(CODE))),
            Event::SoftBreak | Event::HardBreak => cell.push(Span::styled(" ", style)),
            _ => {}
        }
    }

    let columns = alignments.len();
    if columns == 0 || width == 0 {
        return Vec::new();
    }
    // Reserve at least one cell for content, even if the caller's indent fills the pane.
    let prefix = fit_table_prefix(prefix, width.saturating_sub(1));
    let available = width.saturating_sub(prefix.width());
    let mut sizes = vec![1; columns];
    for row in &rows {
        for (size, cell) in sizes.iter_mut().zip(row) {
            *size = (*size).max(cell.iter().map(Span::width).sum());
        }
    }
    let mut lines = Vec::new();
    // Separators would consume the entire pane: retain all cells by stacking them.
    if available < columns.saturating_mul(4).saturating_add(1) {
        for row in rows {
            for cell in row {
                for line in wrap_table_cell(&cell, available) {
                    let mut spans = vec![Span::raw(prefix.clone())];
                    spans.extend(line.spans);
                    lines.push(Line::from(spans));
                }
            }
        }
        return lines;
    }
    let budget = available - (columns * 3 + 1);
    while sizes.iter().sum::<usize>() > budget {
        let largest = sizes.iter().enumerate().max_by_key(|(_, size)| *size);
        if let Some((index, _)) = largest {
            sizes[index] -= 1;
        }
    }
    for (row_index, row) in rows.iter().enumerate() {
        let cells: Vec<_> = sizes
            .iter()
            .enumerate()
            .map(|(index, size)| {
                wrap_table_cell(row.get(index).map(Vec::as_slice).unwrap_or(&[]), *size)
            })
            .collect();
        let height = cells.iter().map(Vec::len).max().unwrap_or(1);
        for y in 0..height {
            let mut spans = vec![Span::raw(prefix.clone()), Span::styled("│", base.fg(RULE))];
            for (index, cell_lines) in cells.iter().enumerate() {
                let line = cell_lines.get(y).cloned().unwrap_or_default();
                let padding = sizes[index].saturating_sub(line.width());
                let left = match alignments[index] {
                    Alignment::Right => padding,
                    Alignment::Center => padding / 2,
                    _ => 0,
                };
                spans.push(Span::raw(" ".repeat(left + 1)));
                spans.extend(line.spans);
                spans.push(Span::raw(" ".repeat(padding - left + 1)));
                spans.push(Span::styled("│", base.fg(RULE)));
            }
            lines.push(Line::from(spans));
        }
        if row_index == 0 {
            let rule = sizes
                .iter()
                .map(|size| "─".repeat(size + 2))
                .collect::<Vec<_>>()
                .join("┼");
            lines.push(Line::from(vec![
                Span::raw(prefix.clone()),
                Span::styled(format!("├{rule}┤"), base.fg(RULE)),
            ]));
        }
    }
    lines
}

fn fit_table_prefix(prefix: &str, width: usize) -> String {
    let mut result = String::new();
    let mut used = 0;
    for ch in prefix.chars() {
        let size = ch.width().unwrap_or(0);
        if used + size > width {
            break;
        }
        result.push(ch);
        used += size;
    }
    result
}

/// Wrap without inserting spaces between inline runs (e.g. `a**b**c`).
fn wrap_table_cell(cell: &[Span<'static>], width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut spans = Vec::new();
    let mut used = 0;
    for span in cell {
        let mut run = String::new();
        for ch in span.content.chars() {
            let size = ch.width().unwrap_or(0);
            if used + size > width && used > 0 {
                if !run.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut run), span.style));
                }
                lines.push(Line::from(std::mem::take(&mut spans)));
                used = 0;
            }
            // A two-cell glyph cannot fit a one-cell column; avoid terminal overflow.
            if size > width {
                run.push('�');
                used += 1;
            } else {
                run.push(ch);
                used += size;
            }
        }
        if !run.is_empty() {
            spans.push(Span::styled(run, span.style));
        }
    }
    lines.push(Line::from(spans));
    lines
}

fn heading_rank(level: HeadingLevel) -> usize {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// Greedy word wrap over styled words, matching `push_block`'s prefix/indent
/// convention so markdown and plain-text cells line up.
fn wrap_words(words: Vec<(String, Style)>, width: usize, prefix: &str) -> Vec<Line<'static>> {
    let indent = " ".repeat(prefix.width());
    let body = width.saturating_sub(prefix.width()).max(1);
    let mut lines = Vec::new();
    let mut spans: Vec<Span<'static>> = vec![Span::raw(prefix.to_string())];
    let mut used = 0usize;
    for (word, style) in words {
        if word.is_empty() {
            continue;
        }
        let length = word.width();
        if used > 0 && used + 1 + length > body {
            lines.push(Line::from(std::mem::take(&mut spans)));
            spans.push(Span::raw(indent.clone()));
            used = 0;
        }
        if length <= body {
            if used > 0 {
                spans.push(Span::raw(" "));
                used += 1;
            }
            spans.push(Span::styled(word, style));
            used += length;
            continue;
        }

        // A URL or inline-code token can exceed the pane by itself. Split it
        // at display-cell boundaries so resizing the terminal reflows it too.
        let mut chunk = String::new();
        let mut chunk_width = 0;
        for ch in word.chars() {
            let char_width = ch.width().unwrap_or(0);
            if chunk_width > 0 && chunk_width + char_width > body {
                spans.push(Span::styled(std::mem::take(&mut chunk), style));
                lines.push(Line::from(std::mem::take(&mut spans)));
                spans.push(Span::raw(indent.clone()));
                chunk_width = 0;
            }
            chunk.push(ch);
            chunk_width += char_width;
        }
        spans.push(Span::styled(chunk, style));
        used = chunk_width;
    }
    lines.push(Line::from(spans));
    lines
}

/// Plain-text wrap for code blocks: no style runs to track, but still breaks
/// inside an overlong token so a long line doesn't push the pane wide.
pub(crate) fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(width.max(1))
        .map(|chunk| chunk.iter().collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_links_keep_the_destination_visible() {
        let lines = render("[Docs](https://example.com/docs)", 80, Style::new(), "");
        assert_eq!(lines[0].to_string(), "Docs (https://example.com/docs)");
    }

    #[test]
    fn tables_align_columns_and_preserve_inline_styles() {
        let lines = render(
            "| left | center | right |\n| :--- | :---: | ---: |\n| a**b**c | *x* | `7` |",
            80,
            Style::new().add_modifier(Modifier::DIM),
            "",
        );
        assert_eq!(lines[2].to_string(), "│ abc  │   x    │     7 │");
        assert!(lines[2].spans.iter().any(|span| {
            span.content == "b"
                && span
                    .style
                    .add_modifier
                    .contains(Modifier::BOLD | Modifier::DIM)
        }));
        assert!(
            lines[2]
                .spans
                .iter()
                .any(|span| span.content == "x"
                    && span.style.add_modifier.contains(Modifier::ITALIC))
        );
        assert!(
            lines[2]
                .spans
                .iter()
                .any(|span| span.content == "7" && span.style.fg == Some(CODE))
        );
    }

    #[test]
    fn tables_fit_unicode_and_every_narrow_width() {
        let text = "| 字 | value |\n| --- | ---: |\n| 界界e\u{301} | abcdefghijkl |\n| | z |";
        for width in 0..40 {
            let lines = render(text, width, Style::new(), "  ");
            assert!(
                lines.iter().all(|line| line.width() <= width),
                "width {width}"
            );
        }
        let lines = render(text, 40, Style::new(), "");
        assert_eq!(lines[0].width(), lines[2].width());
        assert!(lines[2].to_string().contains("界界e\u{301}"));
    }

    #[test]
    fn stacked_tables_keep_content_and_surrounding_prose() {
        let lines = render(
            "before\n\n| a | b |\n| --- | --- |\n| cd | ef |\n\nafter",
            6,
            Style::new(),
            "",
        );
        let text = lines.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(text, ["before", "", "a", "b", "cd", "ef", "", "after"]);
    }

    #[test]
    fn agent_prose_reflows_with_terminal_width() {
        let text = "one two three four five six";
        let narrow = render(text, 10, Style::new(), "");
        let wide = render(text, 30, Style::new(), "");

        assert!(narrow.len() > wide.len());
        assert!(narrow.iter().all(|line| line.width() <= 10));
        assert_eq!(wide.len(), 1);
    }

    #[test]
    fn long_inline_token_fits_the_resized_pane() {
        let text = "`abcdefghijklmno`";
        for width in [5, 8, 20] {
            let lines = render(text, width, Style::new(), "");
            assert!(lines.iter().all(|line| line.width() <= width));
            assert_eq!(
                lines.iter().map(ToString::to_string).collect::<String>(),
                "abcdefghijklmno"
            );
        }
    }
}
