//! Rendering agent prose (`Cell::Assistant`, `Cell::Thinking`) as styled lines.
//!
//! grok-build carries its own multi-thousand-line markdown/mermaid/LaTeX
//! renderer for this; keke's transcript only needs the everyday subset a model
//! actually emits — headings, emphasis, code, lists, quotes — so this stays a
//! thin `pulldown-cmark` walk rather than a port. Malformed or plain-text input
//! renders as prose either way; `pulldown-cmark` never errors.

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
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut lines = Vec::new();
    let mut words: Vec<(String, Style)> = Vec::new();
    let mut style_stack = vec![base];
    let mut list_stack: Vec<Option<u64>> = Vec::new();
    let mut prefix = lead.to_string();
    let mut in_code_block = false;
    let mut code_block = String::new();

    let flush = |lines: &mut Vec<Line<'static>>, words: &mut Vec<(String, Style)>, prefix: &str| {
        if !words.is_empty() {
            lines.extend(wrap_words(std::mem::take(words), width, prefix));
        }
    };

    for event in Parser::new_ext(text, options) {
        let style = *style_stack.last().unwrap_or(&base);
        match event {
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
    let indent = " ".repeat(prefix.chars().count());
    let body = width.saturating_sub(prefix.chars().count()).max(1);
    let mut lines = Vec::new();
    let mut spans: Vec<Span<'static>> = vec![Span::raw(prefix.to_string())];
    let mut used = 0usize;
    for (word, style) in words {
        if word.is_empty() {
            continue;
        }
        let length = word.chars().count();
        if used > 0 && used + 1 + length > body {
            lines.push(Line::from(std::mem::take(&mut spans)));
            spans.push(Span::raw(indent.clone()));
            used = 0;
        }
        if used > 0 {
            spans.push(Span::raw(" "));
            used += 1;
        }
        spans.push(Span::styled(word, style));
        used += length;
    }
    lines.push(Line::from(spans));
    lines
}

/// Plain-text wrap for code blocks: no style runs to track, but still breaks
/// inside an overlong token so a long line doesn't push the pane wide.
fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(width.max(1))
        .map(|chunk| chunk.iter().collect())
        .collect()
}
