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
    // `status` decides whether this is a repository, not `rev-parse HEAD`: a
    // repository that has just been initialized has no HEAD commit yet, and it
    // is exactly the one an agent is about to make the first commit in.
    let status = git(root, &["status", "--porcelain"])?;
    // `symbolic-ref` names the branch whether or not it has a commit yet; only
    // a detached HEAD has no symbolic name, and then git's own "HEAD" says so.
    let branch = git(root, &["symbolic-ref", "--short", "-q", "HEAD"])
        .or_else(|| git(root, &["rev-parse", "--abbrev-ref", "HEAD"]))
        .unwrap_or_default();

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

#[cfg(test)]
mod tests {
    use super::*;

    fn git_in(dir: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.test"])
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn a_repository_without_a_commit_is_still_a_repository() {
        let dir = tempfile::tempdir().expect("tempdir");
        git_in(dir.path(), &["init", "-q", "-b", "trunk"]);
        std::fs::write(dir.path().join("new.txt"), "x").expect("write");

        let status = vcs_status(&AbsPath::new(dir.path()).expect("absolute"))
            .expect("an unborn branch is a repository");

        assert_eq!(status.branch.as_deref(), Some("trunk"));
        assert_eq!(status.changes, ["?? new.txt"]);
    }

    #[test]
    fn a_plain_directory_is_not_a_repository() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            vcs_status(&AbsPath::new(dir.path()).expect("absolute")),
            None
        );
    }
}
