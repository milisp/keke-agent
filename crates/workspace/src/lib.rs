//! The execution host.
//!
//! Everything the engine knows about the project it is working in: where the
//! root is, which files are relevant, what the VCS says, and what project-level
//! instructions apply. Tools do their own I/O; this crate exists for the things
//! the *engine* needs in order to assemble a turn.
//!
//! The containment rule lives here and is not optional: a path that escapes the
//! workspace root is an error, never a clamped path. Silently clamping turns a
//! traversal attempt into a successful read of the wrong file.

mod context;
mod exec;
mod vcs;

pub use context::InstructionFile;
pub use context::discover_instructions;
pub use exec::CommandOutcome;
pub use exec::run_command;
pub use vcs::VcsStatus;
pub use vcs::vcs_status;

use keke_paths::AbsPath;
use keke_paths::RelPath;

/// Why a workspace operation failed.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    /// The path resolved outside the workspace root.
    #[error("{path} is outside the workspace root {root}")]
    Escapes { path: String, root: String },
    #[error("{path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Path(#[from] keke_paths::PathError),
    #[error("command timed out after {millis}ms")]
    Timeout { millis: u64 },
}

/// The project being worked in.
#[derive(Clone, Debug)]
pub struct Workspace {
    root: AbsPath,
}

impl Workspace {
    #[must_use]
    pub fn new(root: AbsPath) -> Self {
        Self { root }
    }

    #[must_use]
    pub fn root(&self) -> &AbsPath {
        &self.root
    }

    /// Resolve a user- or model-supplied path against the workspace root.
    ///
    /// Symlinks are resolved before the containment check when the path exists,
    /// so a symlink pointing outside the workspace cannot be used to step past
    /// the root. A path that does not exist yet is checked lexically, which is
    /// what lets a tool create a new file.
    pub fn resolve(&self, path: &str) -> Result<AbsPath, WorkspaceError> {
        let candidate = std::path::Path::new(path);
        let joined = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.root.as_path().join(candidate)
        };

        let resolved = match std::fs::canonicalize(&joined) {
            Ok(real) => AbsPath::new(real)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                AbsPath::new(normalize_lexically(&joined))?
            }
            Err(source) => {
                return Err(WorkspaceError::Io {
                    path: joined.display().to_string(),
                    source,
                });
            }
        };

        // The root itself may be a symlink; compare against its real form.
        let root = std::fs::canonicalize(self.root.as_path())
            .ok()
            .and_then(|real| AbsPath::new(real).ok())
            .unwrap_or_else(|| self.root.clone());

        if resolved.is_contained_in(&root) {
            Ok(resolved)
        } else {
            Err(WorkspaceError::Escapes {
                path: resolved.to_string(),
                root: root.to_string(),
            })
        }
    }

    /// Express an absolute path relative to the root, for display.
    pub fn relativize(&self, path: &AbsPath) -> Result<RelPath, WorkspaceError> {
        path.strip_root(&self.root).map_err(WorkspaceError::Path)
    }
}

/// Collapse `.` and `..` without touching the filesystem.
///
/// Used only for paths that do not exist yet; existing paths go through
/// `canonicalize`, which also resolves symlinks.
fn normalize_lexically(path: &std::path::Path) -> std::path::PathBuf {
    use std::path::Component;

    let mut out = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn workspace() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().expect("tempdir");
        // macOS hands out `/var/...` which is a symlink to `/private/var`;
        // canonicalizing here keeps the containment comparisons honest.
        let root = std::fs::canonicalize(dir.path()).expect("canonicalize");
        let workspace = Workspace::new(AbsPath::new(root).expect("absolute"));
        (dir, workspace)
    }

    #[test]
    fn resolves_a_relative_path_under_the_root() {
        let (_dir, workspace) = workspace();
        let resolved = workspace.resolve("src/lib.rs").expect("resolves");
        assert!(resolved.is_contained_in(workspace.root()));
    }

    #[test]
    fn rejects_traversal_out_of_the_root() {
        let (_dir, workspace) = workspace();
        let error = workspace.resolve("../../etc/passwd").expect_err("rejected");
        assert!(matches!(error, WorkspaceError::Escapes { .. }), "{error}");
    }

    #[test]
    fn rejects_an_absolute_path_outside_the_root() {
        let (_dir, workspace) = workspace();
        #[cfg(unix)]
        let outside = "/etc/hosts";
        #[cfg(windows)]
        let outside = r"C:\Windows\win.ini";
        let error = workspace.resolve(outside).expect_err("rejected");
        assert!(matches!(error, WorkspaceError::Escapes { .. }), "{error}");
    }

    /// A symlink is the interesting case: the lexical path stays inside the
    /// root while the real target does not.
    #[cfg(unix)]
    #[test]
    fn rejects_a_symlink_pointing_out_of_the_root() {
        let (dir, workspace) = workspace();
        std::os::unix::fs::symlink("/etc", dir.path().join("escape")).expect("symlink");
        let error = workspace.resolve("escape/hosts").expect_err("rejected");
        assert!(matches!(error, WorkspaceError::Escapes { .. }), "{error}");
    }

    #[test]
    fn instructions_are_outermost_first() {
        let (dir, workspace) = workspace();
        let nested = dir.path().join("crates").join("thing");
        std::fs::create_dir_all(&nested).expect("mkdir");
        std::fs::write(dir.path().join("AGENTS.md"), "project").expect("write");
        std::fs::write(nested.join("AGENTS.md"), "nested").expect("write");

        let found = workspace.instructions(&nested).expect("discovers");
        let texts: Vec<&str> = found.iter().map(|file| file.text.as_str()).collect();
        assert_eq!(texts, vec!["project", "nested"]);
    }

    /// A repo carrying both names should contribute its guidance once.
    #[test]
    fn one_directory_yields_at_most_one_instruction_file() {
        let (dir, workspace) = workspace();
        std::fs::write(dir.path().join("AGENTS.md"), "agents").expect("write");
        std::fs::write(dir.path().join("CLAUDE.md"), "claude").expect("write");

        let found = workspace.instructions(dir.path()).expect("discovers");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].text, "agents");
    }

    #[tokio::test]
    async fn run_command_captures_output_and_exit_code() {
        let (_dir, workspace) = workspace();
        let outcome = run_command(
            "sh",
            &["-c", "echo out; echo err 1>&2; exit 3"],
            workspace.root(),
            Duration::from_secs(10),
        )
        .await
        .expect("runs");

        assert_eq!(outcome.exit_code, Some(3));
        assert!(!outcome.succeeded());
        assert_eq!(outcome.stdout.trim(), "out");
        assert_eq!(outcome.stderr.trim(), "err");
    }

    #[tokio::test]
    async fn a_slow_command_times_out_rather_than_failing() {
        let (_dir, workspace) = workspace();
        let error = run_command(
            "sh",
            &["-c", "sleep 30"],
            workspace.root(),
            Duration::from_millis(150),
        )
        .await
        .expect_err("times out");

        assert!(matches!(error, WorkspaceError::Timeout { .. }), "{error}");
    }
}
