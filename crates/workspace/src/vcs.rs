//! Version control state.
//!
//! The engine puts a compact summary of this in the turn context so the model
//! knows what branch it is on and what is already modified. It shells out to
//! `git` rather than linking a git library: the summary is small, the cost is
//! one process per turn, and it always agrees with what the person sees when
//! they run git themselves.

use keke_paths::AbsPath;

/// A summary of the working tree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VcsStatus {
    pub branch: Option<String>,
    /// Porcelain status lines, capped — a repository mid-rebase can have
    /// thousands, and the model needs the shape, not the enumeration.
    pub changes: Vec<String>,
    /// How many lines were dropped by the cap.
    pub truncated: usize,
}

/// The most status lines worth showing the model.
const MAX_STATUS_LINES: usize = 40;

/// Read the working tree state, or `None` when `root` is not a git repository.
///
/// Not being in a repository is normal, not an error: keke must work in a plain
/// directory.
pub fn vcs_status(root: &AbsPath) -> Option<VcsStatus> {
    let branch = git(root, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let status = git(root, &["status", "--porcelain"])?;

    let mut changes: Vec<String> = status.lines().map(str::to_string).collect();
    let truncated = changes.len().saturating_sub(MAX_STATUS_LINES);
    changes.truncate(MAX_STATUS_LINES);

    Some(VcsStatus {
        branch: (!branch.is_empty()).then_some(branch),
        changes,
        truncated,
    })
}

fn git(root: &AbsPath, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(root.as_path())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}
