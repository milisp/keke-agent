//! Operating-system confinement for the commands a model runs.
//!
//! A shell line can reach anything the process can, so the only boundary that
//! holds is one the kernel enforces: Seatbelt on macOS, Landlock plus seccomp
//! on Linux. Checking a command's text is a speed bump, and the guards in
//! `keke-tools` say as much about themselves.
//!
//! Everywhere else — and wherever the kernel lacks the feature — a mode other
//! than `danger_full_access` is refused at construction rather than quietly
//! run unconfined (`AGENTS.md` invariant 8). A setting that claims a boundary
//! it does not enforce is worse than no setting: it is the reason nobody
//! looked.
//!
//! Reads are not narrowed. Every mode may read the whole disk, as codex's
//! profiles do, because a dependency's source under `~/.cargo` is ordinary
//! code the model has reason to open. What is confined is writing and the
//! network — the two ways a command changes something or sends something
//! away.
//!
//! Known limit: a writable root is writable all the way down, `.git/hooks`
//! included, and a hook written there runs unconfined the next time the
//! person commits. Landlock can only grant, never carve an exception out of a
//! grant, so closing that on one platform would leave the two disagreeing
//! about what `workspace_write` means.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod seatbelt;

use std::ffi::OsStr;
use std::ffi::OsString;
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

/// Builds the processes a model's commands run in.
///
/// Holding one means the confinement it describes was checked to be
/// enforceable when it was made; [`Sandbox::command`] cannot fail afterwards,
/// so no call site has an error path in which to run the command bare.
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
    /// binary. Only Linux needs one; elsewhere it is ignored.
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
        let mut command = match &self.launcher {
            Launcher::Unconfined => {
                let mut command = Command::new(program);
                command.args(args);
                command
            }
            #[cfg(target_os = "macos")]
            Launcher::Seatbelt => seatbelt::command(
                &self.writable_roots(workspace),
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
                for root in self.writable_roots(workspace) {
                    command.arg("--write").arg(root);
                }
                command.arg("--").arg(program).args(args);
                command
            }
        };
        command.current_dir(workspace.as_path());
        command
    }

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
    fn writable_roots(&self, workspace: &AbsPath) -> Vec<PathBuf> {
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
        let mut roots: Vec<PathBuf> = Vec::new();
        for path in wanted {
            if let Ok(real) = path.canonicalize()
                && !roots.contains(&real)
            {
                roots.push(real);
            }
        }
        roots
    }
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

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn platform_launcher(
    _policy: &SandboxPolicy,
    _helper: Option<PathBuf>,
) -> Result<Launcher, String> {
    Err(format!("keke has no sandbox for {}", std::env::consts::OS))
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
        assert_eq!(command.get_program(), "sh");
    }

    #[test]
    fn read_only_grants_no_writable_root() {
        let sandbox = Sandbox {
            policy: SandboxPolicy {
                mode: SandboxMode::ReadOnly,
                network_access: true,
                writable_roots: vec![AbsPath::new("/").expect("abs")],
            },
            launcher: Launcher::Unconfined,
        };
        let (_dir, root) = workspace();
        assert!(sandbox.writable_roots(&root).is_empty());
        assert!(!sandbox.network(), "read_only ignores network_access");
    }

    #[test]
    fn workspace_write_resolves_and_deduplicates_its_roots() {
        let (_dir, root) = workspace();
        let sandbox = Sandbox {
            policy: SandboxPolicy {
                mode: SandboxMode::WorkspaceWrite,
                network_access: false,
                writable_roots: vec![
                    root.clone(),
                    AbsPath::new("/definitely/not/here").expect("abs"),
                ],
            },
            launcher: Launcher::Unconfined,
        };
        let roots = sandbox.writable_roots(&root);
        assert_eq!(roots[0], root.as_path());
        assert_eq!(
            roots.iter().filter(|r| *r == root.as_path()).count(),
            1,
            "the workspace is listed once"
        );
        assert!(
            roots.iter().all(|r| r.exists()),
            "a missing root is dropped: {roots:?}"
        );
    }
}
