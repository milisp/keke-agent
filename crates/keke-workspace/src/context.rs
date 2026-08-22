//! Project instruction files.
//!
//! A project states how it wants to be worked on in an `AGENTS.md`. keke also
//! reads `CLAUDE.md`, because a great many repositories already have one and
//! asking people to duplicate it would be a worse experience than reading it.
//!
//! Files are discovered from the workspace root down to the working directory,
//! so a subdirectory's instructions apply on top of the project's.

use std::path::Path;

use keke_paths::AbsPath;

use crate::Workspace;
use crate::WorkspaceError;

/// The filenames checked in each directory, in precedence order. The first one
/// found in a directory wins, so a repo with both does not get its guidance
/// counted twice.
const INSTRUCTION_FILENAMES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];

/// Instructions found in one directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstructionFile {
    pub path: AbsPath,
    pub text: String,
}

impl Workspace {
    /// Instruction files applying to `cwd`, outermost first.
    pub fn instructions(&self, cwd: &Path) -> Result<Vec<InstructionFile>, WorkspaceError> {
        discover_instructions(self.root(), cwd)
    }
}

/// Walk from `root` down to `cwd`, collecting instruction files.
///
/// Returned outermost-first so a caller concatenating them gets the project's
/// general guidance before a subdirectory's specific overrides.
pub fn discover_instructions(
    root: &AbsPath,
    cwd: &Path,
) -> Result<Vec<InstructionFile>, WorkspaceError> {
    let mut chain = Vec::new();
    let mut cursor = Some(cwd);
    while let Some(dir) = cursor {
        chain.push(dir.to_path_buf());
        if dir == root.as_path() {
            break;
        }
        cursor = dir.parent();
    }
    chain.reverse();

    let mut found = Vec::new();
    for dir in chain {
        for name in INSTRUCTION_FILENAMES {
            let path = dir.join(name);
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    found.push(InstructionFile {
                        path: AbsPath::new(&path)?,
                        text,
                    });
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(WorkspaceError::Io {
                        path: path.display().to_string(),
                        source,
                    });
                }
            }
        }
    }
    Ok(found)
}
