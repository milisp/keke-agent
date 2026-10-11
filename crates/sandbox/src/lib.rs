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
//! Reads are unrestricted by default, because dependency sources in shared
//! caches are ordinary code the model has reason to open. On macOS, explicit
//! `deny_read` paths carve credential files out of that grant. Linux refuses
//! read exclusions it cannot enforce rather than quietly ignoring them.
//!
//! Inside a writable root, the metadata that could hand a command more than
//! the sandbox gives it stays read-only: `.keke` (the project's own
//! configuration), `.agents`, and the parts of the workspace's `.git` that
//! Git executes or obeys — `hooks`, `config`, `info`, and `modules` (a
//! submodule's own hooks and config). A hook or a `core.hooksPath` written
//! there would run unconfined at the person's next commit. The rest of the
//! workspace's `.git` is writable, as Claude Code's sandbox leaves it, so
//! `git add` and `git commit` are ordinary commands rather than escapes. A
//! `.git` in any other writable root stays read-only whole. Seatbelt enforces
//! those exceptions. Landlock cannot carve an exception out of a grant, so on
//! Linux those directories are as writable as the rest of the workspace.

mod cache;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod linux_broker;
#[cfg(target_os = "linux")]
mod linux_namespace;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod proxy;
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
    #[error(
        "sandbox_mode \"{requested}\" is looser than the configured \"{ceiling}\", which a \
         session may narrow but not widen"
    )]
    ExceedsCeiling {
        requested: &'static str,
        ceiling: &'static str,
    },
}

/// Workspace metadata a command may read but not write, beneath every
/// writable root: codex's `.git` / `.agents` / `.codex`, with keke's own
/// directory in place of codex's.
///
/// Public so a tool writing from keke's own process, where no sandbox reaches,
/// can hold the same line when deciding whether a write is confined.
pub const PROTECTED_NAMES: &[&str] = &[".git", ".agents", ".keke"];

/// Reviews a previously unapproved domain for one command's proxy.
///
/// Implementers may block waiting for a decision, but must return promptly on
/// command cancellation: proxy shutdown joins all requests before releasing
/// this callback. Returning true grants only the exact domain, not its children.
pub trait NetworkAuthorizer: Send + Sync {
    fn authorize(&self, domain: &str) -> bool;
}

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
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    proxy: Option<std::sync::Arc<proxy::Proxy>>,
    #[cfg(target_os = "linux")]
    broker: Option<std::sync::Arc<linux_broker::Broker>>,
    local_network_ports: Vec<u16>,
    local_listener_ports: Vec<u16>,
    local_network_sockets: Vec<String>,
}

#[derive(Clone, Debug)]
enum Launcher {
    Unconfined,
    #[cfg(target_os = "macos")]
    Seatbelt,
    #[cfg(target_os = "linux")]
    Landlock {
        helper: PathBuf,
        namespace_launcher: Option<PathBuf>,
    },
}

#[cfg(test)]
#[test]
fn scoped_write_roots_are_immutable_and_missing_roots_fail() {
    let dir = tempfile::tempdir().expect("directory");
    let base = Sandbox::new(
        SandboxPolicy::default(),
        Some(std::env::current_exe().expect("exe")),
    )
    .expect("sandbox");
    let scoped = base
        .with_additional_write_roots(vec![dir.path().to_str().expect("utf8").into()])
        .expect("scope");
    assert!(base.policy.writable_roots.is_empty());
    assert_eq!(
        scoped.policy.writable_roots.last().expect("root").as_path(),
        dir.path().canonicalize().expect("canonical")
    );
    assert!(
        base.with_additional_write_roots(vec![
            dir.path().join("missing").to_str().expect("utf8").into()
        ])
        .is_err()
    );
    assert!(!dir.path().join("missing").exists());
}

impl Sandbox {
    /// Add existing canonical directories to a command's write policy without
    /// changing the session policy. Metadata protections use the normal roots.
    pub fn with_additional_write_roots(&self, paths: Vec<String>) -> Result<Self, SandboxError> {
        let invalid = |reason: String| SandboxError::Unavailable {
            mode: self.policy.mode.as_str(),
            reason,
        };
        let mut roots = Vec::new();
        for path in paths {
            if path.contains('\0') || !std::path::Path::new(&path).is_absolute() {
                return Err(invalid(
                    "additional write roots must be absolute without NUL".into(),
                ));
            }
            let real = std::fs::canonicalize(&path)
                .map_err(|error| invalid(format!("additional write root {path}: {error}")))?;
            if !real.is_dir() {
                return Err(invalid(format!(
                    "additional write root {path} is not a directory"
                )));
            }
            roots.push(AbsPath::new(real).map_err(|error| invalid(error.to_string()))?);
        }
        let mut scoped = self.clone();
        if !roots.is_empty() && self.policy.mode == SandboxMode::ReadOnly {
            return Err(invalid(
                "additional write roots require workspace-write".into(),
            ));
        }
        if self.confines()
            && self.policy.mode == SandboxMode::WorkspaceWrite
            && cfg!(any(target_os = "macos", target_os = "linux"))
        {
            scoped.policy.writable_roots.extend(roots);
        }
        Ok(scoped)
    }

    /// Grant outbound access to exact Unix socket paths for one command without
    /// granting filesystem writes. Unsupported modes and platforms fail closed.
    pub fn with_local_network_sockets(&self, paths: Vec<String>) -> Result<Self, SandboxError> {
        if paths
            .iter()
            .any(|path| !std::path::Path::new(path).is_absolute() || path.contains('\0'))
        {
            return Err(SandboxError::Unavailable {
                mode: self.policy.mode.as_str(),
                reason: "Unix socket paths must be absolute and contain no NUL".into(),
            });
        }
        let supported = (cfg!(target_os = "macos")
            || self.policy.linux_namespace_launcher.is_some())
            && self.policy.mode == SandboxMode::WorkspaceWrite
            && self.confines();
        if !paths.is_empty() && !supported {
            return Err(SandboxError::Unavailable {
                mode: self.policy.mode.as_str(),
                reason: "scoped Unix socket access requires workspace-write on macOS or a Linux namespace launcher".into(),
            });
        }
        // Linux enforces exact grants with Landlock ABI 9; fail before spawn when absent.
        #[cfg(target_os = "linux")]
        if !paths.is_empty() {
            linux::probe_unix_socket_grants().map_err(|reason| SandboxError::Unavailable {
                mode: self.policy.mode.as_str(),
                reason,
            })?;
        }
        let mut scoped = self.clone();
        scoped.local_network_sockets = paths;
        scoped.local_network_sockets.sort();
        scoped.local_network_sockets.dedup();
        Ok(scoped)
    }

    /// Grant direct loopback TCP access to explicitly approved ports for this
    /// command only. This can reach host services, not merely workspace servers.
    /// Unsupported confined policies fail closed rather than ignoring a grant.
    pub fn with_local_network_ports(&self, ports: Vec<u16>) -> Result<Self, SandboxError> {
        if ports.contains(&0) {
            return Err(SandboxError::Unavailable {
                mode: self.policy.mode.as_str(),
                reason: "local network ports must be in 1..=65535".into(),
            });
        }
        if !ports.is_empty()
            && self.confines()
            && !((cfg!(target_os = "macos") || self.policy.linux_namespace_launcher.is_some())
                && self.policy.mode == SandboxMode::WorkspaceWrite)
        {
            return Err(SandboxError::Unavailable {
                mode: self.policy.mode.as_str(),
                reason: "scoped local TCP client access requires macOS workspace-write".into(),
            });
        }
        let mut scoped = self.clone();
        scoped.local_network_ports = ports;
        scoped.local_network_ports.sort_unstable();
        scoped.local_network_ports.dedup();
        #[cfg(target_os = "linux")]
        scoped.refresh_broker()?;
        Ok(scoped)
    }

    #[cfg(target_os = "linux")]
    fn refresh_broker(&mut self) -> Result<(), SandboxError> {
        if self.policy.linux_namespace_launcher.is_none() || self.network() {
            return Ok(());
        }
        let mut ports = self.local_network_ports.clone();
        if let Some(proxy) = &self.proxy {
            ports.push(proxy.port());
        }
        ports.sort_unstable();
        ports.dedup();
        self.broker = if ports.is_empty() && self.local_listener_ports.is_empty() {
            None
        } else {
            Some(std::sync::Arc::new(
                linux_broker::Broker::new(ports, self.local_listener_ports.clone(), &self.policy)
                    .map_err(|error| SandboxError::Unavailable {
                    mode: self.policy.mode.as_str(),
                    reason: format!("starting Linux network broker: {error}"),
                })?,
            ))
        };
        Ok(())
    }

    /// Expose named namespace development-server ports on host loopback for
    /// this command only. This does not grant access to host client services.
    pub fn with_local_listener_ports(&self, ports: Vec<u16>) -> Result<Self, SandboxError> {
        if ports.contains(&0)
            || (!ports.is_empty()
                && (self.policy.mode != SandboxMode::WorkspaceWrite
                    || !self.policy.allow_local_binding))
        {
            return Err(SandboxError::Unavailable { mode: self.policy.mode.as_str(), reason: "listener ports require workspace-write with local binding enabled and nonzero ports".into() });
        }
        let mut scoped = self.clone();
        scoped.local_listener_ports = ports;
        scoped.local_listener_ports.sort_unstable();
        scoped.local_listener_ports.dedup();
        #[cfg(target_os = "linux")]
        {
            if !scoped.local_listener_ports.is_empty()
                && scoped.policy.linux_namespace_launcher.is_none()
            {
                return Err(SandboxError::Unavailable {
                    mode: self.policy.mode.as_str(),
                    reason: "Linux host-exposed listener ports require namespace confinement"
                        .into(),
                });
            }
            scoped.refresh_broker()?;
        }
        Ok(scoped)
    }

    /// Create a command-scoped proxy without widening filesystem or network
    /// confinement. Only macOS workspace-write with network access disabled
    /// supports dynamic domain grants; other policies and platforms are unchanged.
    pub fn with_network_authorizer(
        &self,
        authorizer: std::sync::Arc<dyn NetworkAuthorizer>,
        approved_domains: Vec<String>,
    ) -> Result<Self, SandboxError> {
        let scoped = self.clone();
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            let mut scoped = scoped;
            if self.policy.mode == SandboxMode::WorkspaceWrite
                && !self.policy.network_access
                && (cfg!(target_os = "macos") || self.policy.linux_namespace_launcher.is_some())
            {
                scoped.proxy = Some(std::sync::Arc::new(
                    proxy::Proxy::new_with_authorizer(&self.policy, authorizer, approved_domains)
                        .map_err(|error| SandboxError::Unavailable {
                        mode: self.policy.mode.as_str(),
                        reason: format!("starting the domain-filtering proxy failed: {error}"),
                    })?,
                ));
            }
            #[cfg(target_os = "linux")]
            scoped.refresh_broker()?;
            Ok(scoped)
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = (authorizer, approved_domains);
            Ok(scoped)
        }
    }

    /// Check that `policy` can be enforced on this machine, and hold it.
    ///
    /// `helper` is the executable that answers [`HELPER_ARG`] — keke's own
    /// binary. Only Linux needs one; elsewhere it is ignored. On a platform
    /// with no sandbox at all this succeeds unenforced; see
    /// [`Sandbox::is_enforced`].
    pub fn new(policy: SandboxPolicy, helper: Option<PathBuf>) -> Result<Self, SandboxError> {
        policy
            .check_domains()
            .map_err(|reason| SandboxError::Unavailable {
                mode: policy.mode.as_str(),
                reason,
            })?;
        if policy.mode == SandboxMode::DangerFullAccess {
            return Ok(Self {
                policy,
                launcher: Launcher::Unconfined,
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                proxy: None,
                #[cfg(target_os = "linux")]
                broker: None,
                local_network_ports: Vec::new(),
                local_listener_ports: Vec::new(),
                local_network_sockets: Vec::new(),
            });
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let proxy =
            if policy.mode == SandboxMode::WorkspaceWrite && !policy.allowed_domains.is_empty() {
                Some(std::sync::Arc::new(proxy::Proxy::new(&policy).map_err(
                    |error| SandboxError::Unavailable {
                        mode: policy.mode.as_str(),
                        reason: format!("starting the domain-filtering proxy failed: {error}"),
                    },
                )?))
            } else {
                None
            };
        match platform_launcher(&policy, helper) {
            Ok(launcher) => {
                let sandbox = Self {
                    policy,
                    launcher,
                    #[cfg(any(target_os = "macos", target_os = "linux"))]
                    proxy,
                    #[cfg(target_os = "linux")]
                    broker: None,
                    local_network_ports: Vec::new(),
                    local_listener_ports: Vec::new(),
                    local_network_sockets: Vec::new(),
                };
                #[cfg(target_os = "linux")]
                {
                    let mut sandbox = sandbox;
                    sandbox.refresh_broker()?;
                    Ok(sandbox)
                }
                #[cfg(not(target_os = "linux"))]
                {
                    Ok(sandbox)
                }
            }
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
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            proxy: None,
            #[cfg(target_os = "linux")]
            broker: None,
            local_network_ports: Vec::new(),
            local_listener_ports: Vec::new(),
            local_network_sockets: Vec::new(),
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
                self.proxy.as_ref().map(|proxy| proxy.port()),
                self.policy.mode == SandboxMode::WorkspaceWrite && self.policy.allow_local_binding,
                &self.local_network_ports,
                &self.local_network_sockets,
                &self.policy.deny_read,
                program,
                args,
            ),
            #[cfg(target_os = "linux")]
            Launcher::Landlock {
                helper,
                namespace_launcher,
            } => {
                if let Some(launcher) = namespace_launcher {
                    let mut command = linux_namespace::command(
                        launcher,
                        &self.policy,
                        &self.writable_roots(workspace),
                        self.network(),
                        helper,
                        self.broker.as_deref(),
                        &self.local_network_sockets,
                        program,
                        args,
                    );
                    if self.policy.mode == SandboxMode::WorkspaceWrite
                        && self.policy.redirect_package_caches
                    {
                        cache::redirect(&mut command, workspace);
                    }
                    command.current_dir(workspace.as_path());
                    self.proxy_environment(&mut command);
                    return command;
                }
                let mut command = Command::new(helper);
                command.arg(HELPER_ARG);
                if self.network() {
                    command.arg("--network");
                }
                // Landlock cannot express the protected subpaths; see the
                // module documentation.
                for root in self.writable_roots(workspace) {
                    command.arg("--write").arg(root.path);
                }
                command.arg("--").arg(program).args(args);
                command
            }
        };
        self.proxy_environment(&mut command);
        if self.policy.mode == SandboxMode::WorkspaceWrite && self.policy.redirect_package_caches {
            cache::redirect(&mut command, workspace);
        }
        command.current_dir(workspace.as_path());
        command
    }

    fn proxy_environment(&self, command: &mut Command) {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if let Some(proxy) = &self.proxy {
            let url = format!("http://127.0.0.1:{}", proxy.port());
            // Git's SSH transport ignores HTTP_PROXY. Route the default SSH
            // invocation through the same gate, without bypassing host-key or
            // agent checks. Explicit shell overrides still face Seatbelt.
            let ssh = format!(
                "/usr/bin/ssh -o 'ProxyCommand=/usr/bin/nc -X connect -x 127.0.0.1:{} %h %p'",
                proxy.port()
            );
            if cfg!(target_os = "macos")
                && std::env::var_os("GIT_SSH_COMMAND").is_none()
                && std::env::var_os("GIT_SSH").is_none()
            {
                command.envs([("GIT_SSH_COMMAND", ssh.as_str())]);
            }
            #[cfg(target_os = "linux")]
            if std::env::var_os("GIT_SSH_COMMAND").is_none()
                && std::env::var_os("GIT_SSH").is_none()
                && let Launcher::Landlock { helper, .. } = &self.launcher
            {
                let path = helper.to_string_lossy().replace('\'', "'\\''");
                let ssh = format!(
                    "/usr/bin/ssh -o \"ProxyCommand='{}' {} --ssh-proxy {} %h %p\"",
                    path,
                    HELPER_ARG,
                    proxy.port()
                );
                command.envs([("GIT_SSH_COMMAND", ssh)]);
            }
            command.envs(
                [
                    "HTTP_PROXY",
                    "HTTPS_PROXY",
                    "ALL_PROXY",
                    "http_proxy",
                    "https_proxy",
                    "all_proxy",
                ]
                .into_iter()
                .map(|key| (key, url.as_str())),
            );
            // Inherited exclusions must not silently skip the proxy.
            // Exclude loopback only when the kernel permits selected ports;
            // all other loopback destinations remain denied by Seatbelt.
            let exclusions = if self.local_network_ports.is_empty() {
                ""
            } else {
                "localhost,127.0.0.1,::1,[::1]"
            };
            command.envs([("NO_PROXY", exclusions), ("no_proxy", exclusions)]);
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let _ = command;
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
    fn writable_roots(&self, workspace: &AbsPath) -> Vec<WritableRoot> {
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
        // A worktree's git directory lives outside the workspace, so it is a
        // root of its own rather than something the workspace grant covers.
        let gitdirs = git_metadata_dirs(workspace.as_path());
        wanted.extend(gitdirs.iter().cloned());
        let mut roots: Vec<WritableRoot> = Vec::new();
        for (index, path) in wanted.into_iter().enumerate() {
            if let Ok(real) = path.canonicalize()
                && !roots.iter().any(|root| root.path == real)
            {
                let mut read_only = protected_beneath(&real, index == 0);
                read_only.retain(|path| !gitdirs.contains(path));
                for metadata in &gitdirs {
                    if metadata.starts_with(&real) {
                        read_only.extend(protected_git_internals(metadata));
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

/// The workspace's own git directories, which commands may write.
///
/// A `.git` file is followed only when the directory it names points back at
/// it, as a real worktree's does: otherwise a repository could name any
/// directory on disk and have it made writable.
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

/// What Git runs or obeys inside a git directory. Protected even before it
/// exists, so a command cannot be the one to create a hook.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn protected_git_internals(gitdir: &Path) -> Vec<PathBuf> {
    ["hooks", "config", "config.worktree", "info", "modules"]
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
    let namespace_launcher = policy
        .linux_namespace_launcher
        .as_ref()
        .map(|path| path.as_path().to_path_buf());
    if let Some(path) = &namespace_launcher {
        linux_namespace::probe(path)?;
    }
    Ok(Launcher::Landlock {
        helper,
        namespace_launcher,
    })
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
        let mut args: Vec<_> = args.into_iter().collect();
        if args.first().is_some_and(|arg| arg == "--relay-supervisor") {
            args.remove(0);
            return linux_broker::supervise(args);
        }
        if args.first().is_some_and(|arg| arg == "--ssh-proxy") {
            args.remove(0);
            return linux_broker::ssh_proxy(args);
        }
        linux::run(args)
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

/// The sandbox a session is running under right now, and the most it may ever
/// be loosened to.
///
/// The configured policy is a *ceiling*: a client (an editor over ACP) may
/// narrow what the operator configured, never widen it, because the person at
/// the other end of a protocol is not the person who wrote the config. The
/// current [`Sandbox`] sits behind a lock so a switch takes effect for the
/// next command without rebuilding the composition.
pub struct SandboxSwitch {
    ceiling: SandboxPolicy,
    helper: Option<PathBuf>,
    current: std::sync::RwLock<std::sync::Arc<Sandbox>>,
}

impl SandboxSwitch {
    /// Start at the ceiling, failing if this machine cannot enforce it.
    pub fn new(policy: SandboxPolicy, helper: Option<PathBuf>) -> Result<Self, SandboxError> {
        let sandbox = Sandbox::new(policy.clone(), helper.clone())?;
        Ok(Self {
            ceiling: policy,
            helper,
            current: std::sync::RwLock::new(std::sync::Arc::new(sandbox)),
        })
    }

    /// A switch over no confinement, for tests and compositions that have
    /// decided so explicitly. Its ceiling is `danger_full_access`.
    #[must_use]
    pub fn unconfined() -> Self {
        Self {
            ceiling: SandboxPolicy::unconfined(),
            helper: None,
            current: std::sync::RwLock::new(std::sync::Arc::new(Sandbox::unconfined())),
        }
    }

    /// The sandbox in force now. Callers take it per use rather than holding
    /// it, so a later [`Self::set`] reaches them.
    #[must_use]
    pub fn current(&self) -> std::sync::Arc<Sandbox> {
        match self.current.read() {
            Ok(guard) => std::sync::Arc::clone(&guard),
            Err(poisoned) => std::sync::Arc::clone(&poisoned.into_inner()),
        }
    }

    #[must_use]
    pub fn mode(&self) -> SandboxMode {
        self.current().policy().mode
    }

    #[must_use]
    pub fn ceiling(&self) -> SandboxMode {
        self.ceiling.mode
    }

    /// Every mode no looser than the ceiling, least latitude first.
    #[must_use]
    pub fn offered(&self) -> Vec<SandboxMode> {
        SandboxMode::ALL
            .into_iter()
            .filter(|mode| !mode.is_looser_than(self.ceiling.mode))
            .collect()
    }

    /// Switch to `mode`, keeping the ceiling's other settings.
    ///
    /// Refused when `mode` is looser than the ceiling, or when this machine
    /// cannot enforce it; the old sandbox stays in force either way, so a
    /// failed switch never leaves a session unconfined.
    pub fn set(&self, mode: SandboxMode) -> Result<(), SandboxError> {
        if mode.is_looser_than(self.ceiling.mode) {
            return Err(SandboxError::ExceedsCeiling {
                requested: mode.as_str(),
                ceiling: self.ceiling.mode.as_str(),
            });
        }
        let policy = SandboxPolicy {
            mode,
            ..self.ceiling.clone()
        };
        let sandbox = std::sync::Arc::new(Sandbox::new(policy, self.helper.clone())?);
        match self.current.write() {
            Ok(mut guard) => *guard = sandbox,
            Err(poisoned) => *poisoned.into_inner() = sandbox,
        }
        Ok(())
    }
}

impl std::fmt::Debug for SandboxSwitch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxSwitch")
            .field("ceiling", &self.ceiling.mode)
            .field("mode", &self.mode())
            .finish()
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
                ..SandboxPolicy::default()
            },
            launcher: Launcher::Unconfined,
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            proxy: None,
            #[cfg(target_os = "linux")]
            broker: None,
            local_network_ports: Vec::new(),
            local_listener_ports: Vec::new(),
            local_network_sockets: Vec::new(),
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
                ..SandboxPolicy::default()
            },
            launcher: Launcher::Unconfined,
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            proxy: None,
            #[cfg(target_os = "linux")]
            broker: None,
            local_network_ports: Vec::new(),
            local_listener_ports: Vec::new(),
            local_network_sockets: Vec::new(),
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
    fn a_repository_is_writable_except_what_git_executes() {
        let (_dir, root) = workspace();
        let gitdir = root.as_path().join(".git");
        std::fs::create_dir(&gitdir).expect("gitdir");
        std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/main\n").expect("head");
        let sandbox = Sandbox {
            policy: SandboxPolicy::default(),
            launcher: Launcher::Unconfined,
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            proxy: None,
            #[cfg(target_os = "linux")]
            broker: None,
            local_network_ports: Vec::new(),
            local_listener_ports: Vec::new(),
            local_network_sockets: Vec::new(),
        };
        let roots = sandbox.writable_roots(&root);
        assert!(!roots[0].read_only.contains(&gitdir));
        for name in ["hooks", "config", "config.worktree", "info", "modules"] {
            assert!(
                roots[0].read_only.contains(&gitdir.join(name)),
                "{name} should stay protected"
            );
        }
        assert!(roots[0].read_only.contains(&root.as_path().join(".keke")));
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
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            proxy: None,
            #[cfg(target_os = "linux")]
            broker: None,
            local_network_ports: Vec::new(),
            local_listener_ports: Vec::new(),
            local_network_sockets: Vec::new(),
        };
        assert!(!sandbox.is_enforced());
        assert!(!sandbox.confines());
        assert!(Sandbox::unconfined().is_enforced(), "nothing was asked for");
    }

    fn switch(mode: SandboxMode) -> SandboxSwitch {
        SandboxSwitch::new(
            SandboxPolicy {
                mode,
                ..SandboxPolicy::default()
            },
            Some(PathBuf::from("/proc/self/exe")),
        )
        .expect("this machine must be able to build the sandbox")
    }

    #[test]
    fn a_session_cannot_widen_the_sandbox_beyond_what_was_configured() {
        let switch = switch(SandboxMode::ReadOnly);

        let error = switch
            .set(SandboxMode::WorkspaceWrite)
            .expect_err("wider than the ceiling");

        let message = error.to_string();
        assert!(message.contains("workspace_write") && message.contains("read_only"));
        assert_eq!(switch.mode(), SandboxMode::ReadOnly, "the old mode stays");
        assert!(switch.set(SandboxMode::DangerFullAccess).is_err());
    }

    #[test]
    fn a_session_can_narrow_the_sandbox_and_is_offered_only_modes_within_the_ceiling() {
        let switch = switch(SandboxMode::WorkspaceWrite);
        assert_eq!(
            switch.offered(),
            vec![SandboxMode::ReadOnly, SandboxMode::WorkspaceWrite]
        );

        switch.set(SandboxMode::ReadOnly).expect("narrowing");
        assert_eq!(switch.mode(), SandboxMode::ReadOnly);
        assert_eq!(switch.current().policy().mode, SandboxMode::ReadOnly);
        assert_eq!(switch.ceiling(), SandboxMode::WorkspaceWrite);

        switch
            .set(SandboxMode::WorkspaceWrite)
            .expect("back up to the ceiling");
        assert_eq!(switch.mode(), SandboxMode::WorkspaceWrite);
    }
}
