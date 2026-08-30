//! Shared plumbing: workspace containment and output budgeting.

use keke_paths::AbsPath;
use keke_paths::RelPath;
use keke_tool::ToolCallContext;
use keke_tool::ToolError;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

/// Longest model-visible payload any tool may return.
///
/// Tool output is charged against the same context budget as the conversation,
/// so a tool that reads a generated 40MB file must degrade rather than end the
/// turn.
pub(crate) const MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// Resolve a model-supplied path against the workspace and refuse escapes.
///
/// Normalization is lexical: `..` is folded here rather than by the filesystem
/// so a path pointing at something that does not exist yet (`write_file`) is
/// checked by the same rule as one that does.
pub(crate) fn resolve(ctx: &ToolCallContext, path: &str) -> Result<AbsPath, ToolError> {
    let raw = Path::new(path);
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        let rel = RelPath::new(raw)
            .map_err(|error| ToolError::custom("bad_path", format!("{path}: {error}")))?;
        ctx.workspace_root.as_path().join(rel.as_path())
    };

    let normalized = AbsPath::new(lexically_normalize(&joined))
        .map_err(|error| ToolError::custom("bad_path", format!("{path}: {error}")))?;

    if !normalized.is_contained_in(&ctx.workspace_root) {
        return Err(ToolError::denied(format!(
            "{path} resolves outside the workspace root {}",
            ctx.workspace_root
        )));
    }
    Ok(normalized)
}

/// How a path reads back to the model: relative to the root when it is inside.
pub(crate) fn display(root: &AbsPath, path: &AbsPath) -> String {
    path.strip_root(root)
        .map(|rel| {
            if rel.as_str().is_empty() {
                ".".to_string()
            } else {
                rel.as_str().to_string()
            }
        })
        .unwrap_or_else(|_| path.as_str().to_string())
}

fn lexically_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            // Popping is safe because a leading `..` leaves the prefix/root in
            // place, which then fails the containment check below.
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Lines added and removed going from `before` to `after`, plus a rendered
/// diff a person can read at a glance.
///
/// `hunk` is never sent to the model — `render()` only quotes `added` and
/// `removed` to keep the model-visible summary terse — but a surface that
/// wants to show what actually changed needs more than a line count, so the
/// rendered lines ride along in [`ToolResult::value`](keke_protocol::ToolResult)
/// for a TUI or log viewer to display.
///
/// Follows the layout `codex-rs`'s diff renderer settled on (line-number
/// gutter sized to the diff's own widest number, `⋮` between hunks, no
/// `---`/`+++`/`@@` unified-diff scaffolding) — with the sign moved in front
/// of the gutter rather than after it. codex keeps its diff as live styled
/// spans inside one process; this one has to survive a JSON round trip
/// through [`ToolResult::value`](keke_protocol::ToolResult), and a context
/// line's sign is a space, which a reader on the other side of that trip
/// cannot tell apart from gutter padding unless the sign comes first.
#[derive(Debug, serde::Serialize)]
pub struct LineDiff {
    pub added: usize,
    pub removed: usize,
    pub hunk: String,
}

struct DiffRow {
    /// `None` marks the `⋮` gap between two hunks.
    marker: Option<char>,
    line_number: Option<usize>,
    text: String,
}

pub(crate) fn line_diff(before: &str, after: &str) -> LineDiff {
    let diff = similar::TextDiff::from_lines(before, after);
    let mut added = 0;
    let mut removed = 0;
    let mut rows: Vec<DiffRow> = Vec::new();
    let mut max_line_number = 0;

    for (group_index, group) in diff.grouped_ops(2).iter().enumerate() {
        if group_index > 0 {
            rows.push(DiffRow {
                marker: None,
                line_number: None,
                text: String::new(),
            });
        }
        for op in group {
            for change in diff.iter_changes(op) {
                let (marker, line_number) = match change.tag() {
                    similar::ChangeTag::Delete => {
                        removed += 1;
                        ('-', change.old_index())
                    }
                    similar::ChangeTag::Insert => {
                        added += 1;
                        ('+', change.new_index())
                    }
                    similar::ChangeTag::Equal => (' ', change.new_index()),
                };
                let line_number = line_number.map(|index| index + 1);
                max_line_number = max_line_number.max(line_number.unwrap_or(0));
                rows.push(DiffRow {
                    marker: Some(marker),
                    line_number,
                    text: change.value().trim_end_matches('\n').to_string(),
                });
            }
        }
    }

    // Sized to this diff's own widest line number, exactly as codex's
    // `line_number_width` does, rather than a column wide enough for any
    // file this tool could ever touch.
    let gutter_width = max_line_number.to_string().len().max(1);
    let mut hunk = String::new();
    for row in &rows {
        match row.marker {
            None => hunk.push_str(&format!("{:gutter_width$}  ⋮\n", "")),
            Some(marker) => {
                let line_number = row
                    .line_number
                    .map_or(String::new(), |number| number.to_string());
                hunk.push_str(&format!(
                    "{marker} {line_number:>gutter_width$} {}\n",
                    row.text
                ));
            }
        }
    }
    let (hunk, _) = cap(hunk, "diff truncated");

    LineDiff {
        added,
        removed,
        hunk,
    }
}

/// Cut `text` to the output budget, appending a note when anything was dropped.
pub(crate) fn cap(text: String, note: &str) -> (String, bool) {
    if text.len() <= MAX_OUTPUT_BYTES {
        return (text, false);
    }
    let mut end = MAX_OUTPUT_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = text[..end].to_string();
    out.push_str("\n… ");
    out.push_str(note);
    (out, true)
}

#[cfg(test)]
mod tests {
    use super::line_diff;

    #[test]
    fn line_diff_carries_the_hunk_text_not_just_counts() {
        let before = "one\ntwo\nthree\n";
        let after = "one\nTWO\nthree\n";

        let diff = line_diff(before, after);

        assert_eq!(diff.added, 1);
        assert_eq!(diff.removed, 1);
        assert!(diff.hunk.contains("- 2 two"), "hunk was:\n{}", diff.hunk);
        assert!(diff.hunk.contains("+ 2 TWO"), "hunk was:\n{}", diff.hunk);
    }

    #[test]
    fn separate_hunks_get_a_gap_marker_like_codexs_diff_view() {
        let before: String = (1..=20).map(|n| format!("line{n}\n")).collect();
        let mut lines: Vec<String> = (1..=20).map(|n| format!("line{n}\n")).collect();
        lines[4] = "CHANGED five\n".to_string();
        lines[14] = "CHANGED fifteen\n".to_string();
        let after: String = lines.concat();

        let diff = line_diff(&before, &after);

        assert!(diff.hunk.contains('⋮'), "hunk was:\n{}", diff.hunk);
        assert!(
            !diff.hunk.contains("line8"),
            "context far from either change must not be dragged in: {}",
            diff.hunk
        );
    }
}
