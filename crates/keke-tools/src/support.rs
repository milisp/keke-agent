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
