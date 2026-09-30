//! The frozen listing that goes in the system prompt.

use crate::store::Entry;

/// Cut `text` to at most `max` bytes without splitting a character.
pub(crate) fn truncate_on_boundary(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn marker(dropped: usize) -> String {
    format!("(truncated — {dropped} more entries; use memory_read)")
}

/// The fragment text, or `None` when the budget is zero.
///
/// Whole entries are dropped from the end rather than lines cut in half, and the
/// marker says how many, so the model knows the listing is partial and that
/// `memory_read` with no name shows the rest. The result never exceeds `budget`.
pub(crate) fn render(dir: &str, entries: &[Entry], budget: usize) -> Option<String> {
    if budget == 0 {
        return None;
    }
    let preamble = format!(
        "You have persistent memory at {dir}. It holds durable facts about the person you work \
         with, the project, and your own role that should survive this session — not scratch \
         notes. Use memory_read to list or read entries and memory_write to save or update one; \
         keep entries short. This list was taken when the session started and does not change \
         while it runs."
    );
    let header = if entries.is_empty() {
        format!("{preamble}\n\nNo memories saved yet.")
    } else {
        format!("{preamble}\n\nSaved memories:")
    };
    let lines: Vec<String> = entries
        .iter()
        .map(|entry| {
            if entry.first_line.is_empty() {
                format!("- {}", entry.name)
            } else {
                format!("- {}: {}", entry.name, entry.first_line)
            }
        })
        .collect();

    // The most entries whose text plus the marker for the rest still fits.
    // Prefix lengths, so this is linear in the number of entries.
    let mut used = Vec::with_capacity(lines.len() + 1);
    used.push(header.len());
    for line in &lines {
        used.push(used.last().copied().unwrap_or(0) + 1 + line.len());
    }
    for keep in (0..=lines.len()).rev() {
        let dropped = lines.len() - keep;
        let cost = used[keep]
            + if dropped > 0 {
                1 + marker(dropped).len()
            } else {
                0
            };
        if cost <= budget {
            let mut text = header;
            for line in &lines[..keep] {
                text.push('\n');
                text.push_str(line);
            }
            if dropped > 0 {
                text.push('\n');
                text.push_str(&marker(dropped));
            }
            return Some(text);
        }
    }
    // Not even the preamble fits: keep its head and say so.
    let tail = format!("\n{}", marker(lines.len()));
    let room = budget.saturating_sub(tail.len());
    let mut text = truncate_on_boundary(&header, room).to_string();
    let left = budget - text.len();
    text.push_str(truncate_on_boundary(&tail, left));
    Some(text)
}
