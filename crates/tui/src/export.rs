//! Writing the scrollback out as a file a person keeps.
//!
//! Rendering is a pure function of the cells so it can be tested without a
//! terminal, and so the one thing that decides what an exported transcript
//! looks like is not spread across the drawing code.

use std::path::Path;
use std::path::PathBuf;

use crate::transcript::CallState;
use crate::transcript::Cell;

/// The scrollback as Markdown.
///
/// The banner is left out: it says what this session was launched with, which
/// is state of the terminal rather than something anyone said. Tool calls keep
/// their outcome, because a transcript that shows a command without saying
/// whether it worked is a record of intent, not of what happened.
#[must_use]
pub(crate) fn markdown(cells: &[Cell]) -> String {
    let mut out = String::new();
    for cell in cells {
        let block = match cell {
            Cell::Banner(_) => continue,
            Cell::User(text) => format!("## User\n\n{}\n", text.trim_end()),
            Cell::Assistant(text) => format!("## Assistant\n\n{}\n", text.trim_end()),
            Cell::Plan(plan) => format!("## Plan\n\n{}\n", plan.text.trim_end()),
            Cell::Notice(text) => format!("> {}\n", text.trim_end().replace('\n', "\n> ")),
            Cell::Error(text) => {
                format!("> **error:** {}\n", text.trim_end().replace('\n', "\n> "))
            }
            Cell::Tool(call) => {
                let state = match call.state {
                    CallState::Running => "running".to_string(),
                    CallState::Finished(status) => format!("{status:?}").to_lowercase(),
                };
                let mut block = format!("- `{}` {} — {state}\n", call.name, call.summary);
                if let Some(detail) = &call.detail {
                    block.push_str(&format!("  - {}\n", detail.trim_end()));
                }
                block
            }
        };
        out.push_str(&block);
        out.push('\n');
    }
    out
}

/// Where `/export <path>` writes, given where the person is sitting.
///
/// An empty argument is refused rather than defaulted: a file written
/// somewhere nobody named is a file nobody finds again.
pub(crate) fn destination(argument: &str, cwd: &Path) -> Result<PathBuf, String> {
    let argument = argument.trim();
    if argument.is_empty() {
        return Err("where to? `/export <path>`".to_string());
    }
    let path = expand_home(argument);
    Ok(if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    })
}

/// `~` is what a person types for their home directory; the shell is not here
/// to expand it, so the command does.
fn expand_home(argument: &str) -> PathBuf {
    let Some(rest) = argument.strip_prefix('~') else {
        return PathBuf::from(argument);
    };
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return PathBuf::from(argument);
    };
    match rest.strip_prefix('/') {
        Some(rest) => home.join(rest),
        None if rest.is_empty() => home,
        // `~other` is another person's home, which we cannot resolve.
        None => PathBuf::from(argument),
    }
}

/// Render the cells and put them on disk.
pub(crate) fn write(cells: &[Cell], path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("creating {}: {error}", parent.display()))?;
    }
    std::fs::write(path, markdown(cells))
        .map_err(|error| format!("writing {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::ToolCell;
    use keke_protocol::ToolCallId;
    use keke_protocol::ToolStatus;

    fn tool() -> Cell {
        Cell::Tool(ToolCell {
            id: ToolCallId::new("call-1"),
            name: "read".to_string(),
            summary: "src/lib.rs".to_string(),
            arguments: "path=src/lib.rs".to_string(),
            state: CallState::Finished(ToolStatus::Ok),
            detail: Some("40 lines".to_string()),
        })
    }

    #[test]
    fn the_banner_is_not_part_of_what_anyone_said() {
        let text = markdown(&[
            Cell::Banner(vec!["keke".to_string()]),
            Cell::User("hi".to_string()),
        ]);
        assert!(!text.contains("keke"));
        assert!(text.contains("## User\n\nhi"));
    }

    /// A command without its outcome is a record of intent, not of what
    /// happened.
    #[test]
    fn a_tool_call_keeps_its_outcome() {
        let text = markdown(&[tool()]);
        assert!(text.contains("`read` src/lib.rs"), "{text}");
        assert!(text.contains("ok"), "{text}");
        assert!(text.contains("40 lines"), "{text}");
    }

    #[test]
    fn a_relative_path_is_resolved_against_the_working_directory() {
        let cwd = Path::new("/work/repo");
        assert_eq!(
            destination("out/log.md", cwd),
            Ok(PathBuf::from("/work/repo/out/log.md"))
        );
        assert_eq!(
            destination("/tmp/log.md", cwd),
            Ok(PathBuf::from("/tmp/log.md"))
        );
    }

    /// A file written somewhere nobody named is a file nobody finds again.
    #[test]
    fn an_unnamed_destination_is_refused() {
        assert!(destination("  ", Path::new("/work")).is_err());
    }
}
