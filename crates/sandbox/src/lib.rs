//! Operating-system confinement for the commands a model runs.
//!
//! A shell line can reach anything the process can, so the only boundary that
//! holds is one the kernel enforces: Seatbelt on macOS, Landlock plus seccomp
//! on Linux. Checking a command's text is a speed bump, and the guards in
//! `keke-tools` say as much about themselves.
//!
//! Where the kernel lacks the feature — Linux without Landlock, macOS without
//! `sandbox-exec` — a mode other than `danger_full_access` is refused at
//! construction rather than quietly run unconfined (`AGENTS.md` invariant 8).
//! A setting that claims a boundary it does not enforce is worse than no
//! setting: it is the reason nobody looked.
//!
//! Windows has no sandbox here, as codex has none by default. What it could
//! have without an administrator — a write-restricted token — confines writes
//! only where no one else already may write, and cannot confine the network
//! at all. So on Windows the sandbox reports itself unenforced
//! ([`Sandbox::is_enforced`]) and the tool pack makes every command a
//! person's decision instead: the boundary is a person, and nothing claims it
//! is a sandbox.
//!
//! Reads are not narrowed. Every mode may read the whole disk, as codex's
//! profiles do, because a dependency's source under `~/.cargo` is ordinary
//! code the model has reason to open. What is confined is writing and the
//! network — the two ways a command changes something or sends something
//! away.
//!
//! Inside a writable root, the metadata that could hand a command more than
//! the sandbox gives it stays read-only, as codex keeps it: `.git` (a hook
//! written there runs unconfined at the next commit), `.keke` (the project's
//! own configuration), and `.agents`. Direct Git staging and commits receive
//! a narrow `.git` write grant while hooks and Git configuration stay
//! protected. Seatbelt enforces those exceptions. Landlock cannot carve an
//! exception out of a grant, so on Linux those directories are as writable as
//! the rest of the workspace.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod seatbelt;

use std::ffi::OsStr;
use std::ffi::OsString;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use keke_config_types::SandboxMode;
use keke_config_types::SandboxPolicy;
use keke_paths::AbsPath;

/// The first argument that turns keke's own binary into the Linux launcher.
///
/// Landlock and seccomp apply to the calling thread and everything it later
/// execs, so they have to be installed in the child, between `fork` and
/// `exec`. Doing that from a `pre_exec` hook means `unsafe` in a multi-threaded
/// process; re-executing a single-threaded copy of keke that installs them and
/// then execs the command needs none. The composition root answers this
/// argument before anything else runs.
pub const HELPER_ARG: &str = "__keke-sandbox";

/// Why a sandbox could not be built.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error(
        "sandbox_mode = \"{mode}\" cannot be enforced here: {reason}. Set sandbox_mode = \
         \"danger_full_access\" in $KEKE_HOME/config.toml to run commands unconfined"
    )]
    Unavailable { mode: &'static str, reason: String },
}

/// Workspace metadata a command may read but not write, beneath every
/// writable root: codex's `.git` / `.agents` / `.codex`, with keke's own
/// directory in place of codex's.
///
/// Public so a tool writing from keke's own process, where no sandbox reaches,
/// can hold the same line when deciding whether a write is confined.
pub const PROTECTED_NAMES: &[&str] = &[".git", ".agents", ".keke"];

/// Builds the processes a model's commands run in.
///
/// Holding one means the confinement it describes was checked to be
/// enforceable when it was made — or, where no sandbox exists, that it says so
/// through [`Sandbox::is_enforced`]. [`Sandbox::command`] cannot fail
/// afterwards, so no call site has an error path in which to run the command
/// bare.
#[derive(Clone, Debug)]
pub struct Sandbox {
    policy: SandboxPolicy,
    launcher: Launcher,
}

#[derive(Clone, Debug)]
enum Launcher {
    Unconfined,
    #[cfg(target_os = "macos")]
    Seatbelt,
    #[cfg(target_os = "linux")]
    Landlock {
        helper: PathBuf,
    },
}

impl Sandbox {
    /// Check that `policy` can be enforced on this machine, and hold it.
    ///
    /// `helper` is the executable that answers [`HELPER_ARG`] — keke's own
    /// binary. Only Linux needs one; elsewhere it is ignored. On a platform
    /// with no sandbox at all this succeeds unenforced; see
    /// [`Sandbox::is_enforced`].
    pub fn new(policy: SandboxPolicy, helper: Option<PathBuf>) -> Result<Self, SandboxError> {
        if policy.mode == SandboxMode::DangerFullAccess {
            return Ok(Self {
                policy,
                launcher: Launcher::Unconfined,
            });
        }
        match platform_launcher(&policy, helper) {
            Ok(launcher) => Ok(Self { policy, launcher }),
            Err(reason) => Err(SandboxError::Unavailable {
                mode: policy.mode.as_str(),
                reason,
            }),
        }
    }

    /// A sandbox that confines nothing, for compositions and tests that have
    /// decided so explicitly.
    #[must_use]
    pub fn unconfined() -> Self {
        Self {
            policy: SandboxPolicy::unconfined(),
            launcher: Launcher::Unconfined,
        }
    }

    #[must_use]
    pub fn policy(&self) -> &SandboxPolicy {
        &self.policy
    }

    /// Whether the policy asks for confinement that commands will not get,
    /// because this platform has no sandbox. A caller holding such a sandbox
    /// must put a person in front of every command instead.
    #[must_use]
    pub fn is_enforced(&self) -> bool {
        self.policy.mode == SandboxMode::DangerFullAccess
            || !matches!(self.launcher, Launcher::Unconfined)
    }

    /// Whether commands run under any confinement at all — false both for
    /// `danger_full_access` and where nothing is enforced.
    #[must_use]
    pub fn confines(&self) -> bool {
        !matches!(self.launcher, Launcher::Unconfined)
    }

    /// A shell running `line`, confined, starting in `workspace`.
    #[must_use]
    pub fn shell(&self, line: &str, workspace: &AbsPath) -> Command {
        let (program, flag) = if cfg!(windows) {
            ("cmd", "/C")
        } else {
            ("sh", "-c")
        };
        self.command(program, [flag, line], workspace)
    }

    /// `program` with `args`, confined, starting in `workspace`.
    ///
    /// The workspace is both the working directory and — under
    /// `workspace_write` — the main writable root, which is why they are one
    /// parameter rather than two that could disagree.
    #[must_use]
    pub fn command<I, S>(&self, program: &str, args: I, workspace: &AbsPath) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.command_with_metadata(program, args, workspace, false)
    }

    /// Run a direct Git operation with repository metadata writable while
    /// preserving the workspace and network sandbox. Callers must restrict
    /// the subcommand and disable hooks; arbitrary shell text is not safe here.
    #[must_use]
    pub fn git_command<I, S>(&self, args: I, workspace: &AbsPath) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.command_with_metadata("git", args, workspace, true)
    }

    fn command_with_metadata<I, S>(
        &self,
        program: &str,
        args: I,
        workspace: &AbsPath,
        git_metadata: bool,
    ) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = match &self.launcher {
            Launcher::Unconfined => {
                let mut command = Command::new(program);
                command.args(args);
                command
            }
            #[cfg(target_os = "macos")]
            Launcher::Seatbelt => seatbelt::command(
                &self.writable_roots_with_git(workspace, git_metadata),
                self.network(),
                program,
                args,
            ),
            #[cfg(target_os = "linux")]
            Launcher::Landlock { helper } => {
                let mut command = Command::new(helper);
                command.arg(HELPER_ARG);
                if self.network() {
                    command.arg("--network");
                }
                // Landlock cannot express the protected subpaths; see the
                // module documentation.
                for root in self.writable_roots_with_git(workspace, git_metadata) {
                    command.arg("--write").arg(root.path);
                }
                command.arg("--").arg(program).args(args);
                command
            }
        };
        command.current_dir(workspace.as_path());
        command
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn network(&self) -> bool {
        match self.policy.mode {
            SandboxMode::WorkspaceWrite => self.policy.network_access,
            SandboxMode::ReadOnly => false,
            SandboxMode::DangerFullAccess => true,
        }
    }

    /// Where a command may write, resolved to real paths.
    ///
    /// Resolved because both kernels judge the path a file actually lives at:
    /// macOS's temporary directory is `/var/folders/…` by name and
    /// `/private/var/folders/…` on disk, and a rule naming the first matches
    /// nothing. A root that does not exist is dropped rather than failing the
    /// command — there is nothing under it to write to.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[cfg(test)]
    fn writable_roots(&self, workspace: &AbsPath) -> Vec<WritableRoot> {
        self.writable_roots_with_git(workspace, false)
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn writable_roots_with_git(
        &self,
        workspace: &AbsPath,
        git_metadata: bool,
    ) -> Vec<WritableRoot> {
        if self.policy.mode != SandboxMode::WorkspaceWrite {
            return Vec::new();
        }
        let mut wanted: Vec<PathBuf> = vec![workspace.as_path().to_path_buf()];
        // Compilers, test runners, and `mktemp` all assume a temporary
        // directory they can write, and nothing the person keeps lives there.
        wanted.push(std::env::temp_dir());
        if cfg!(unix) {
            wanted.push(PathBuf::from("/tmp"));
        }
        wanted.extend(
            self.policy
                .writable_roots
                .iter()
                .map(|root| root.as_path().to_path_buf()),
        );
        let gitdirs = if git_metadata {
            git_metadata_dirs(workspace.as_path())
        } else {
            Vec::new()
        };
        wanted.extend(gitdirs.iter().cloned());
        let mut roots: Vec<WritableRoot> = Vec::new();
        for (index, path) in wanted.into_iter().enumerate() {
            if let Ok(real) = path.canonicalize()
                && !roots.iter().any(|root| root.path == real)
            {
                let mut read_only = protected_beneath(&real, index == 0);
                if git_metadata {
                    read_only.retain(|path| !gitdirs.contains(path));
                    for metadata in &gitdirs {
                        if metadata.starts_with(&real) {
                            read_only.extend(protected_git_internals(metadata));
                        }
                    }
                }
                roots.push(WritableRoot {
                    path: real,
                    read_only,
                });
            }
        }
        roots
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn git_metadata_dirs(workspace: &Path) -> Vec<PathBuf> {
    let dot_git = workspace.join(".git");
    if dot_git.is_symlink() {
        return Vec::new();
    }
    let Some(gitdir) = (if dot_git.is_dir() {
        dot_git.canonicalize().ok()
    } else {
        pointed_gitdir(workspace, &dot_git).filter(|gitdir| {
            let backlink = std::fs::read_to_string(gitdir.join("gitdir"))
                .ok()
                .and_then(|pointer| gitdir.join(pointer.trim()).canonicalize().ok());
            backlink.is_some() && backlink == dot_git.canonicalize().ok()
        })
    }) else {
        return Vec::new();
    };
    if !gitdir.join("HEAD").is_file() {
        return Vec::new();
    }
    let mut dirs = vec![gitdir.clone()];
    if let Ok(common) = std::fs::read_to_string(gitdir.join("commondir"))
        && let Ok(path) = gitdir.join(common.trim()).canonicalize()
        && path != gitdir
        && path.join("objects").is_dir()
        && path.join("config").is_file()
    {
        dirs.push(path);
    }
    dirs
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn protected_git_internals(gitdir: &Path) -> Vec<PathBuf> {
    ["hooks", "config", "config.worktree", "info"]
        .into_iter()
        .map(|name| gitdir.join(name))
        .collect()
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
/// A root a command may write beneath, less the paths it may not.
#[derive(Clone, Debug, PartialEq, Eq)]
struct WritableRoot {
    path: PathBuf,
    // Landlock cannot carve these out of a grant; see the module docs.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    read_only: Vec<PathBuf>,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
/// The metadata under `root` that stays read-only.
///
/// Only what exists is protected — a `git init` in a fresh directory has
/// nothing yet to escalate through — except `.keke` in the workspace itself,
/// which is protected before it exists so the model cannot be the one to
/// create a project's configuration. A `.git` *file* is a worktree's or a
/// submodule's pointer, and the directory it names is where the hooks live.
fn protected_beneath(root: &Path, is_workspace: bool) -> Vec<PathBuf> {
    let mut protected = Vec::new();
    for name in PROTECTED_NAMES {
        let path = root.join(name);
        if path.exists() || (is_workspace && *name == ".keke") {
            protected.push(path.canonicalize().unwrap_or(path.clone()));
        }
        if *name == ".git"
            && path.is_file()
            && let Some(gitdir) = pointed_gitdir(root, &path)
        {
            protected.push(gitdir);
        }
    }
    protected
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn pointed_gitdir(root: &Path, pointer: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(pointer).ok()?;
    let target = text
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))?
        .trim();
    root.join(target).canonicalize().ok()
}

#[cfg(target_os = "macos")]
fn platform_launcher(
    _policy: &SandboxPolicy,
    _helper: Option<PathBuf>,
) -> Result<Launcher, String> {
    if std::path::Path::new(seatbelt::SANDBOX_EXEC).is_file() {
        Ok(Launcher::Seatbelt)
    } else {
        Err(format!("{} is missing", seatbelt::SANDBOX_EXEC))
    }
}

#[cfg(target_os = "linux")]
fn platform_launcher(policy: &SandboxPolicy, helper: Option<PathBuf>) -> Result<Launcher, String> {
    let helper = helper.ok_or_else(|| "no launcher executable was configured".to_string())?;
    linux::probe(policy)?;
    Ok(Launcher::Landlock { helper })
}

/// No sandbox: the result reports itself unenforced, and the tool pack puts a
/// person in front of every command instead.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn platform_launcher(
    _policy: &SandboxPolicy,
    _helper: Option<PathBuf>,
) -> Result<Launcher, String> {
    Ok(Launcher::Unconfined)
}

/// Run as the Linux launcher: confine this process, then become the command.
///
/// `args` are what followed [`HELPER_ARG`]. Returns only on failure, and the
/// caller must then exit non-zero without running anything — a launcher that
/// could not confine and ran the command anyway would be the silent
/// downgrade this crate exists to refuse.
pub fn run_helper(args: impl IntoIterator<Item = OsString>) -> std::io::Error {
    #[cfg(target_os = "linux")]
    {
        linux::run(args.into_iter().collect())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = args.into_iter();
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "the sandbox launcher only exists on Linux",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> (tempfile::TempDir, AbsPath) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = AbsPath::new(dir.path().canonicalize().expect("canonicalize")).expect("abs");
        (dir, root)
    }

    #[test]
    fn danger_full_access_is_always_available() {
        let sandbox = Sandbox::new(SandboxPolicy::unconfined(), None).expect("never refused");
        let (_dir, root) = workspace();
        let command = sandbox.shell("true", &root);
        let shell = if cfg!(windows) { "cmd" } else { "sh" };
        assert_eq!(command.get_program(), shell);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn read_only_grants_no_writable_root() {
        let sandbox = Sandbox {
            policy: SandboxPolicy {
                mode: SandboxMode::ReadOnly,
                auto_approve_bash: true,
                network_access: true,
                writable_roots: vec![AbsPath::new("/").expect("abs")],
            },
            launcher: Launcher::Unconfined,
        };
        let (_dir, root) = workspace();
        assert!(sandbox.writable_roots(&root).is_empty());
        assert!(!sandbox.network(), "read_only ignores network_access");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn workspace_write_resolves_and_deduplicates_its_roots() {
        let (_dir, root) = workspace();
        let sandbox = Sandbox {
            policy: SandboxPolicy {
                mode: SandboxMode::WorkspaceWrite,
                auto_approve_bash: true,
                network_access: false,
                writable_roots: vec![
                    root.clone(),
                    AbsPath::new("/definitely/not/here").expect("abs"),
                ],
            },
            launcher: Launcher::Unconfined,
        };
        let roots = sandbox.writable_roots(&root);
        assert_eq!(roots[0].path, root.as_path());
        assert_eq!(
            roots.iter().filter(|r| r.path == root.as_path()).count(),
            1,
            "the workspace is listed once"
        );
        assert!(
            roots.iter().all(|r| r.path.exists()),
            "a missing root is dropped: {roots:?}"
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn metadata_that_exists_is_protected_and_keke_is_protected_before_it_does() {
        let (_dir, root) = workspace();
        std::fs::create_dir(root.as_path().join(".git")).expect("mkdir");
        let protected = protected_beneath(root.as_path(), true);
        assert!(protected.contains(&root.as_path().join(".git")));
        assert!(protected.contains(&root.as_path().join(".keke")));
        assert!(
            !protected.contains(&root.as_path().join(".agents")),
            "nothing to protect"
        );
        assert!(
            protected_beneath(root.as_path(), false)
                .iter()
                .all(|path| !path.ends_with(".keke")),
            "only the workspace's own configuration is protected before it exists"
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    /// A worktree's `.git` is a file naming the real git directory, which is
    /// where a hook would be written.
    #[test]
    fn a_worktree_pointer_protects_the_directory_it_names() {
        let (_dir, root) = workspace();
        let real = root.as_path().join("elsewhere");
        std::fs::create_dir(&real).expect("mkdir");
        std::fs::write(root.as_path().join(".git"), "gitdir: elsewhere\n").expect("write");
        let protected = protected_beneath(root.as_path(), false);
        assert!(protected.contains(&real), "{protected:?}");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn scoped_git_writes_keep_hooks_and_configuration_protected() {
        let (_dir, root) = workspace();
        let gitdir = root.as_path().join(".git");
        std::fs::create_dir(&gitdir).expect("gitdir");
        std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/main\n").expect("head");
        let sandbox = Sandbox {
            policy: SandboxPolicy::default(),
            launcher: Launcher::Unconfined,
        };
        let ordinary = sandbox.writable_roots(&root);
        assert!(ordinary[0].read_only.contains(&gitdir));
        let scoped = sandbox.writable_roots_with_git(&root, true);
        assert!(!scoped[0].read_only.contains(&gitdir));
        for name in ["hooks", "config", "config.worktree", "info"] {
            assert!(
                scoped[0].read_only.contains(&gitdir.join(name)),
                "{name} should stay protected"
            );
        }
        assert!(scoped[0].read_only.contains(&root.as_path().join(".keke")));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn a_git_pointer_needs_a_backlink_before_it_receives_write_access() {
        let (_dir, root) = workspace();
        let gitdir = root.as_path().join("git-data");
        std::fs::create_dir(&gitdir).expect("gitdir");
        std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/main\n").expect("head");
        std::fs::write(root.as_path().join(".git"), "gitdir: git-data\n").expect("pointer");
        assert!(git_metadata_dirs(root.as_path()).is_empty());
        std::fs::write(
            gitdir.join("gitdir"),
            root.as_path().join(".git").display().to_string(),
        )
        .expect("backlink");
        assert_eq!(git_metadata_dirs(root.as_path()), vec![gitdir]);
    }

    /// Where there is no sandbox the value says so, rather than posing as
    /// one that works.
    #[test]
    fn an_unconfined_launcher_under_a_confining_mode_is_unenforced() {
        let sandbox = Sandbox {
            policy: SandboxPolicy::default(),
            launcher: Launcher::Unconfined,
        };
        assert!(!sandbox.is_enforced());
        assert!(!sandbox.confines());
        assert!(Sandbox::unconfined().is_enforced(), "nothing was asked for");
    }
}
