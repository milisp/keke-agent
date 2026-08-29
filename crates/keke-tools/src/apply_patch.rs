//! The `apply_patch` tool: a whole changeset in one call.
//!
//! The patch language is the file-oriented envelope OpenAI's codex and xAI's
//! grok-build both put in front of their models (`*** Begin Patch` … `*** End
//! Patch`). keke speaks it because frontier models have been trained on it, not
//! because keke prefers it — the grammar here is implemented from the published
//! format description rather than copied, so nothing is vendored.
//!
//! The value over `edit` is atomicity across files: a patch that touches five
//! files either lands whole or leaves the workspace untouched, so a run that
//! fails halfway cannot leave a half-renamed module behind.

use keke_protocol::ContentBlock;
use keke_tool::ListToolsContext;
use keke_tool::Tool;
use keke_tool::ToolCallContext;
use keke_tool::ToolCapabilities;
use keke_tool::ToolDescription;
use keke_tool::ToolError;
use keke_tool::ToolId;
use keke_tool::ToolKind;
use keke_tool::ToolOutput;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

use crate::support;

const BEGIN: &str = "*** Begin Patch";
const END: &str = "*** End Patch";
const ADD: &str = "*** Add File: ";
const DELETE: &str = "*** Delete File: ";
const UPDATE: &str = "*** Update File: ";
const MOVE: &str = "*** Move to: ";
const EOF_MARKER: &str = "*** End of File";

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ApplyPatchArgs {
    /// The full patch, from `*** Begin Patch` through `*** End Patch`.
    pub patch: String,
}

/// What one file section did.
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Updated,
    Deleted,
}

#[derive(Debug, Serialize)]
pub struct FileChange {
    pub path: String,
    pub kind: ChangeKind,
    /// Set when the section carried a `*** Move to:` line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moved_to: Option<String>,
    pub diff: support::LineDiff,
}

#[derive(Debug, Serialize)]
pub struct ApplyPatchOutput {
    pub changes: Vec<FileChange>,
}

impl ToolOutput for ApplyPatchOutput {
    fn render(&self) -> Vec<ContentBlock> {
        let mut out = String::new();
        for change in &self.changes {
            let verb = match change.kind {
                ChangeKind::Added => "added",
                ChangeKind::Updated => "updated",
                ChangeKind::Deleted => "deleted",
            };
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("{verb} {}", change.path));
            if let Some(moved_to) = &change.moved_to {
                out.push_str(&format!(" -> {moved_to}"));
            }
            out.push_str(&format!(
                " (+{} -{})",
                change.diff.added, change.diff.removed
            ));
        }
        if out.is_empty() {
            out.push_str("patch applied (no files changed)");
        }
        vec![ContentBlock::text(out)]
    }
}

/// Applies a multi-file patch in the `*** Begin Patch` envelope.
pub struct ApplyPatch;

impl Tool for ApplyPatch {
    type Args = ApplyPatchArgs;
    type Output = ApplyPatchOutput;

    fn id(&self) -> ToolId {
        ToolId::new("apply_patch")
    }

    fn description(&self, _ctx: &ListToolsContext) -> ToolDescription {
        ToolDescription::new(
            "Apply a multi-file patch atomically. The patch is wrapped in `*** Begin Patch` and \
             `*** End Patch`, and holds one or more file sections: `*** Add File: <path>` \
             followed by `+` lines, `*** Delete File: <path>`, or `*** Update File: <path>` \
             optionally followed by `*** Move to: <new path>` and then hunks. A hunk opens with \
             `@@` (optionally naming the enclosing function or class) and its lines are prefixed \
             ` ` for context, `-` to remove, `+` to add; end a hunk with `*** End of File` when \
             it matches the end of the file. Give three lines of context around each change. \
             Nothing is written unless every section applies.",
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::of_kind(ToolKind::Edit)
    }

    async fn run(&self, ctx: ToolCallContext, args: Self::Args) -> Result<Self::Output, ToolError> {
        let sections = parse(&args.patch)?;

        // Resolve and compute every section before touching the filesystem, so
        // a section that cannot apply aborts the whole patch rather than
        // leaving earlier sections written.
        let mut planned = Vec::with_capacity(sections.len());
        for section in sections {
            planned.push(plan(&ctx, section).await?);
        }

        let mut changes = Vec::with_capacity(planned.len());
        for step in planned {
            changes.push(commit(step).await?);
        }
        Ok(ApplyPatchOutput { changes })
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
enum Section {
    Add {
        path: String,
        contents: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        hunks: Vec<Hunk>,
    },
}

/// One line of a hunk, keeping which side of the change it belongs to.
#[derive(Debug, PartialEq, Eq)]
enum Op {
    Context(String),
    Remove(String),
    Add(String),
}

/// One `@@` block: the lines it expects to find and the lines it leaves behind.
#[derive(Debug, PartialEq, Eq)]
struct Hunk {
    /// Text after `@@`, used to jump to the right region when context repeats.
    header: Option<String>,
    ops: Vec<Op>,
    /// The hunk carried `*** End of File`, so it anchors at the file's end.
    at_eof: bool,
}

fn bad_patch(message: impl Into<String>) -> ToolError {
    ToolError::custom("bad_patch", message)
}

fn parse(patch: &str) -> Result<Vec<Section>, ToolError> {
    let body = patch.trim_matches(['\n', '\r']);
    let mut lines = body.lines().peekable();

    match lines.next() {
        Some(first) if first.trim_end() == BEGIN => {}
        _ => return Err(bad_patch(format!("patch must start with `{BEGIN}`"))),
    }

    let mut sections = Vec::new();
    let mut saw_end = false;
    while let Some(line) = lines.next() {
        let line = line.trim_end_matches('\r');
        if line.trim_end() == END {
            saw_end = true;
            break;
        }
        if let Some(path) = line.strip_prefix(ADD) {
            let mut contents = String::new();
            while let Some(next) = lines.peek() {
                let next = next.trim_end_matches('\r');
                let Some(added) = next.strip_prefix('+') else {
                    break;
                };
                contents.push_str(added);
                contents.push('\n');
                lines.next();
            }
            sections.push(Section::Add {
                path: path.trim().to_string(),
                contents,
            });
        } else if let Some(path) = line.strip_prefix(DELETE) {
            sections.push(Section::Delete {
                path: path.trim().to_string(),
            });
        } else if let Some(path) = line.strip_prefix(UPDATE) {
            let mut move_to = None;
            if let Some(next) = lines.peek()
                && let Some(target) = next.trim_end_matches('\r').strip_prefix(MOVE)
            {
                move_to = Some(target.trim().to_string());
                lines.next();
            }
            let mut hunks = Vec::new();
            while let Some(next) = lines.peek() {
                let next = next.trim_end_matches('\r');
                if next.starts_with("*** ") && next.trim_end() != EOF_MARKER {
                    break;
                }
                let Some(header) = next.strip_prefix("@@") else {
                    return Err(bad_patch(format!(
                        "`{UPDATE}{}`: expected a hunk starting with `@@`, found `{next}`",
                        path.trim()
                    )));
                };
                let header = header.trim().to_string();
                lines.next();
                hunks.push(parse_hunk(
                    (!header.is_empty()).then_some(header),
                    &mut lines,
                )?);
            }
            if hunks.is_empty() {
                return Err(bad_patch(format!("`{UPDATE}{}` has no hunks", path.trim())));
            }
            sections.push(Section::Update {
                path: path.trim().to_string(),
                move_to,
                hunks,
            });
        } else if line.trim().is_empty() {
            // A blank line between sections is slop, not an error.
        } else {
            return Err(bad_patch(format!(
                "expected a file section header, found `{line}`"
            )));
        }
    }

    if !saw_end {
        return Err(bad_patch(format!("patch must end with `{END}`")));
    }
    if sections.is_empty() {
        return Err(bad_patch("patch contains no file sections"));
    }
    Ok(sections)
}

fn parse_hunk<'a>(
    header: Option<String>,
    lines: &mut std::iter::Peekable<impl Iterator<Item = &'a str>>,
) -> Result<Hunk, ToolError> {
    let mut hunk = Hunk {
        header,
        ops: Vec::new(),
        at_eof: false,
    };
    while let Some(next) = lines.peek() {
        let next = next.trim_end_matches('\r');
        if next.starts_with("@@") {
            break;
        }
        if next.trim_end() == EOF_MARKER {
            hunk.at_eof = true;
            lines.next();
            break;
        }
        if next.starts_with("*** ") {
            break;
        }
        lines.next();
        match next.chars().next() {
            Some('+') => hunk.ops.push(Op::Add(next[1..].to_string())),
            Some('-') => hunk.ops.push(Op::Remove(next[1..].to_string())),
            Some(' ') => hunk.ops.push(Op::Context(next[1..].to_string())),
            // Models routinely strip the marker from a blank context line;
            // rejecting the patch over it would be pedantry, not safety.
            None => hunk.ops.push(Op::Context(String::new())),
            Some(_) => {
                return Err(bad_patch(format!(
                    "hunk line must start with ' ', '-' or '+', found `{next}`"
                )));
            }
        }
    }
    if hunk.ops.is_empty() {
        return Err(bad_patch("hunk is empty"));
    }
    Ok(hunk)
}

impl Hunk {
    /// The lines this hunk expects to find, in file order.
    fn before(&self) -> Vec<String> {
        self.ops
            .iter()
            .filter_map(|op| match op {
                Op::Context(line) | Op::Remove(line) => Some(line.clone()),
                Op::Add(_) => None,
            })
            .collect()
    }

    /// The lines this hunk leaves behind, given what it actually matched.
    ///
    /// Context lines are taken from `matched` rather than from the patch: a
    /// match may have been whitespace-insensitive, and the file's indentation
    /// is the truth. Only `+` lines come from the model.
    fn after(&self, matched: &[String]) -> Vec<String> {
        let mut consumed = 0;
        let mut out = Vec::with_capacity(self.ops.len());
        for op in &self.ops {
            match op {
                Op::Context(line) => {
                    out.push(
                        matched
                            .get(consumed)
                            .cloned()
                            .unwrap_or_else(|| line.clone()),
                    );
                    consumed += 1;
                }
                Op::Remove(_) => consumed += 1,
                Op::Add(line) => out.push(line.clone()),
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Applying
// ---------------------------------------------------------------------------

/// A section resolved against the workspace and reduced to bytes to write.
struct Step {
    change: FileChange,
    write: Option<(keke_paths::AbsPath, String)>,
    remove: Option<keke_paths::AbsPath>,
}

async fn plan(ctx: &ToolCallContext, section: Section) -> Result<Step, ToolError> {
    match section {
        Section::Add { path, contents } => {
            let resolved = support::resolve(ctx, &path)?;
            let display = support::display(&ctx.workspace_root, &resolved);
            if resolved.as_path().exists() {
                return Err(bad_patch(format!("{display}: already exists")));
            }
            Ok(Step {
                change: FileChange {
                    path: display,
                    kind: ChangeKind::Added,
                    moved_to: None,
                    diff: support::line_diff("", &contents),
                },
                write: Some((resolved, contents)),
                remove: None,
            })
        }
        Section::Delete { path } => {
            let resolved = support::resolve(ctx, &path)?;
            let display = support::display(&ctx.workspace_root, &resolved);
            let previous = read(&resolved, &display).await?;
            Ok(Step {
                change: FileChange {
                    path: display,
                    kind: ChangeKind::Deleted,
                    moved_to: None,
                    diff: support::line_diff(&previous, ""),
                },
                write: None,
                remove: Some(resolved),
            })
        }
        Section::Update {
            path,
            move_to,
            hunks,
        } => {
            let resolved = support::resolve(ctx, &path)?;
            let display = support::display(&ctx.workspace_root, &resolved);
            let previous = read(&resolved, &display).await?;
            let updated = apply_hunks(&previous, &hunks, &display)?;

            let (target, moved_to) = match move_to {
                Some(destination) => {
                    let target = support::resolve(ctx, &destination)?;
                    let shown = support::display(&ctx.workspace_root, &target);
                    if target != resolved && target.as_path().exists() {
                        return Err(bad_patch(format!("{shown}: already exists")));
                    }
                    (target, Some(shown))
                }
                None => (resolved.clone(), None),
            };
            let remove = (target != resolved).then_some(resolved);

            Ok(Step {
                change: FileChange {
                    path: display,
                    kind: ChangeKind::Updated,
                    moved_to,
                    diff: support::line_diff(&previous, &updated),
                },
                write: Some((target, updated)),
                remove,
            })
        }
    }
}

async fn commit(step: Step) -> Result<FileChange, ToolError> {
    let Step {
        change,
        write,
        remove,
    } = step;
    if let Some(path) = remove {
        tokio::fs::remove_file(path.as_path())
            .await
            .map_err(|error| {
                ToolError::custom("write_failed", format!("{}: {error}", change.path))
            })?;
    }
    if let Some((path, contents)) = write {
        if let Some(parent) = path.as_path().parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|error| {
                ToolError::custom("write_failed", format!("{}: {error}", change.path))
            })?;
        }
        tokio::fs::write(path.as_path(), contents.as_bytes())
            .await
            .map_err(|error| {
                ToolError::custom("write_failed", format!("{}: {error}", change.path))
            })?;
    }
    Ok(change)
}

async fn read(path: &keke_paths::AbsPath, display: &str) -> Result<String, ToolError> {
    tokio::fs::read_to_string(path.as_path())
        .await
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => {
                ToolError::custom("file_not_found", format!("{display}: no such file"))
            }
            _ => ToolError::custom("read_failed", format!("{display}: {error}")),
        })
}

fn split_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    text.strip_suffix('\n')
        .unwrap_or(text)
        .split('\n')
        .map(str::to_string)
        .collect()
}

fn apply_hunks(previous: &str, hunks: &[Hunk], display: &str) -> Result<String, ToolError> {
    let mut lines = split_lines(previous);
    let mut cursor = 0usize;

    for hunk in hunks {
        // The `@@ header` names the enclosing function or class; honouring it
        // is what makes a patch land in the right one of several identical
        // bodies.
        let mut from = cursor;
        if let Some(header) = &hunk.header
            && let Some(index) = lines[cursor..]
                .iter()
                .position(|line| line.trim() == header.trim())
        {
            from = cursor + index + 1;
        }

        let old = hunk.before();
        let at = seek(&lines, &old, from, hunk.at_eof).ok_or_else(|| {
            let sample = old
                .iter()
                .find(|line| !line.trim().is_empty())
                .map(String::as_str)
                .unwrap_or("<blank>");
            ToolError::custom(
                "no_match",
                format!("{display}: hunk context not found, starting at `{sample}`"),
            )
        })?;

        let new = hunk.after(&lines[at..at + old.len()]);
        cursor = at + new.len();
        lines.splice(at..at + old.len(), new);
    }

    let mut out = lines.join("\n");
    // A file that ended with a newline still does; one that did not, still
    // does not. Rewriting either way would show up as spurious diff noise.
    if !out.is_empty() && (previous.is_empty() || previous.ends_with('\n')) {
        out.push('\n');
    }
    Ok(out)
}

/// Find `pattern` in `lines` at or after `start`, loosening whitespace as it goes.
///
/// Models reproduce context lines from memory, so re-indented or
/// trailing-whitespace-trimmed context is the common case, not an anomaly. Each
/// pass is tried across the whole range before the next relaxes further, so an
/// exact match always wins over a fuzzy one.
fn seek(lines: &[String], pattern: &[String], start: usize, at_eof: bool) -> Option<usize> {
    if pattern.is_empty() {
        return Some(start.min(lines.len()));
    }
    if pattern.len() > lines.len() {
        return None;
    }
    let last = lines.len() - pattern.len();
    let from = if at_eof {
        last.max(start).min(last)
    } else {
        start
    };
    if from > last {
        return None;
    }

    let passes: [fn(&str) -> &str; 3] = [|line| line, str::trim_end, str::trim];
    for normalize in passes {
        for at in from..=last {
            if pattern
                .iter()
                .enumerate()
                .all(|(offset, want)| normalize(&lines[at + offset]) == normalize(want))
            {
                return Some(at);
            }
        }
    }
    // An end-of-file hunk that did not match at the end may still match
    // earlier; a mid-file hunk has already searched everything it can.
    if at_eof && from > start {
        return seek(lines, pattern, start, false);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(error: &ToolError) -> &str {
        match error {
            ToolError::Execution { code, .. } => code,
            other => panic!("expected an execution failure, got {other:?}"),
        }
    }

    fn hunks_of(patch: &str) -> Vec<Hunk> {
        match parse(patch).expect("parses").pop().expect("one section") {
            Section::Update { hunks, .. } => hunks,
            other => panic!("expected an update section, got {other:?}"),
        }
    }

    /// The grammar keke tells the model to write must be the grammar keke
    /// parses. Nothing else keeps the prompt and the parser from drifting
    /// apart, and the drift would show up only as patches the model cannot
    /// land.
    #[test]
    fn the_example_patch_in_the_prompt_parses() {
        let doc = crate::prompt::APPLY_PATCH_FORMAT;
        let start = doc.rfind(BEGIN).expect("the prompt shows a full example");
        let end = doc.rfind(END).expect("the example is closed") + END.len();

        let sections = parse(&doc[start..end]).expect("the documented example parses");

        assert!(matches!(
            sections.as_slice(),
            [
                Section::Add { .. },
                Section::Update {
                    move_to: Some(_),
                    ..
                },
                Section::Delete { .. },
            ]
        ));
    }

    #[test]
    fn a_patch_must_be_wrapped_in_its_envelope() {
        let error = parse("*** Update File: a.txt\n@@\n-a\n+b\n").expect_err("no envelope");
        assert_eq!(code(&error), "bad_patch");
    }

    #[test]
    fn an_add_section_takes_every_following_plus_line() {
        let sections = parse("*** Begin Patch\n*** Add File: a.txt\n+one\n+two\n*** End Patch\n")
            .expect("parses");
        assert_eq!(
            sections,
            vec![Section::Add {
                path: "a.txt".to_string(),
                contents: "one\ntwo\n".to_string(),
            }]
        );
    }

    #[test]
    fn a_context_line_belongs_to_both_sides_of_a_hunk() {
        let hunks = hunks_of(
            "*** Begin Patch\n*** Update File: a.txt\n@@\n keep\n-drop\n+take\n*** End Patch\n",
        );
        assert_eq!(
            hunks[0].before(),
            vec!["keep".to_string(), "drop".to_string()]
        );
        assert_eq!(
            hunks[0].after(&hunks[0].before()),
            vec!["keep".to_string(), "take".to_string()]
        );
    }

    #[test]
    fn a_hunk_header_is_kept_for_locating_the_region() {
        let hunks = hunks_of(
            "*** Begin Patch\n*** Update File: a.txt\n@@ fn second\n-a\n+b\n*** End Patch\n",
        );
        assert_eq!(hunks[0].header.as_deref(), Some("fn second"));
    }

    #[test]
    fn an_end_of_file_marker_anchors_the_hunk_at_the_end() {
        let hunks = hunks_of(
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-a\n+b\n*** End of File\n*** End Patch\n",
        );
        assert!(hunks[0].at_eof);
    }

    #[test]
    fn a_header_disambiguates_two_identical_bodies() {
        let previous = "fn first\n    body\nfn second\n    body\n";
        let hunks = hunks_of(
            "*** Begin Patch\n*** Update File: a.rs\n@@ fn second\n-    body\n+    changed\n*** End Patch\n",
        );
        let updated = apply_hunks(previous, &hunks, "a.rs").expect("applies");
        assert_eq!(updated, "fn first\n    body\nfn second\n    changed\n");
    }

    #[test]
    fn a_context_line_keeps_the_files_indentation_not_the_patchs() {
        let previous = "    alpha\n    beta\n";
        let hunks = hunks_of(
            "*** Begin Patch\n*** Update File: a.txt\n@@\n alpha\n-beta\n+gamma\n*** End Patch\n",
        );
        let updated = apply_hunks(previous, &hunks, "a.txt").expect("applies");
        assert_eq!(updated, "    alpha\ngamma\n");
        // `alpha` was matched loosely but written back with its own indent.
    }

    #[test]
    fn an_exact_match_wins_over_a_whitespace_insensitive_one() {
        let previous = "  x\nx\n";
        let hunks =
            hunks_of("*** Begin Patch\n*** Update File: a.txt\n@@\n-x\n+y\n*** End Patch\n");
        let updated = apply_hunks(previous, &hunks, "a.txt").expect("applies");
        assert_eq!(updated, "  x\ny\n");
    }

    #[test]
    fn a_file_without_a_trailing_newline_keeps_none() {
        let hunks =
            hunks_of("*** Begin Patch\n*** Update File: a.txt\n@@\n-a\n+b\n*** End Patch\n");
        assert_eq!(apply_hunks("a", &hunks, "a.txt").expect("applies"), "b");
    }

    #[test]
    fn unmatched_context_names_the_line_it_looked_for() {
        let hunks =
            hunks_of("*** Begin Patch\n*** Update File: a.txt\n@@\n-missing\n+b\n*** End Patch\n");
        let error = apply_hunks("something else\n", &hunks, "a.txt").expect_err("no match");
        assert_eq!(code(&error), "no_match");
    }
}
