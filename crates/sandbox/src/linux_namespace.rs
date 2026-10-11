//! Trusted namespace bootstrap. Landlock remains the write boundary; mount
//! overlays provide exclusions that an additive Landlock ruleset cannot express.

use std::ffi::OsStr;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use keke_config_types::SandboxPolicy;

use crate::WritableRoot;

fn trusted_executable(path: &Path) -> Result<(), String> {
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("namespace launcher {}: {error}", path.display()))?;
    let metadata = canonical.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
        return Err("namespace launcher must be an executable file".into());
    }
    // A user-writable launcher would turn cloning a repository into selecting
    // the executable responsible for enforcing its own confinement.
    for item in canonical.ancestors() {
        let metadata = item.metadata().map_err(|error| error.to_string())?;
        if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err(format!(
                "namespace launcher and ancestors must be root-owned and not group/world writable: {}",
                item.display()
            ));
        }
    }
    if metadata.mode() & 0o6000 != 0 {
        return Err("setuid/setgid namespace launchers are not supported".into());
    }
    Ok(())
}

fn bootstrap(launcher: &Path, network: bool) -> Command {
    let mut command = Command::new(launcher);
    command.args([
        "--unshare-user",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        "--die-with-parent",
        "--new-session",
        "--cap-drop",
        "ALL",
        "--ro-bind",
        "/",
        "/",
        // Inheriting host /proc exposes other mount namespaces via pid/root
        // and open FDs. Use an empty proc until a fresh mount is available.
        "--tmpfs",
        "/proc",
        "--dev",
        "/dev",
    ]);
    if !network {
        command.arg("--unshare-net");
    }
    command
}

pub(crate) fn probe(launcher: &Path) -> Result<(), String> {
    trusted_executable(launcher)?;
    let output = bootstrap(launcher, false)
        .args(["--", "/bin/true"])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("starting namespace launcher: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "namespace confinement is unavailable: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

// Keep the independent permission inputs explicit at this enforcement boundary.
#[allow(clippy::too_many_arguments)]
pub(crate) fn command<I, S>(
    launcher: &Path,
    policy: &SandboxPolicy,
    roots: &[WritableRoot],
    network: bool,
    helper: &Path,
    broker: Option<&crate::linux_broker::Broker>,
    unix_sockets: &[String],
    program: &str,
    args: I,
) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = bootstrap(launcher, network);
    // Bind outer roots first; otherwise a later /tmp grant could cover a
    // workspace's read-only overlay and silently undo the exclusion.
    let mut ordered: Vec<_> = roots.iter().collect();
    ordered.sort_by_key(|root| root.path.components().count());
    for root in &ordered {
        command.arg("--bind").arg(&root.path).arg(&root.path);
    }
    let mut protected: Vec<PathBuf> = roots
        .iter()
        .flat_map(|root| root.read_only.iter().cloned())
        .collect();
    protected.sort();
    protected.dedup();
    for path in protected {
        if path.exists() {
            command.arg("--ro-bind").arg(&path).arg(&path);
        } else {
            command
                .arg("--perms")
                .arg("000")
                .arg("--tmpfs")
                .arg(&path)
                .arg("--remount-ro")
                .arg(&path);
        }
    }
    for denied in &policy.deny_read {
        let original = denied.as_path();
        let canonical = original.canonicalize().unwrap_or_else(|_| original.into());
        for path in [original, canonical.as_path()] {
            if path.is_file() {
                command.arg("--ro-bind").arg("/dev/null").arg(path);
            } else {
                command
                    .arg("--perms")
                    .arg("000")
                    .arg("--tmpfs")
                    .arg(path)
                    .arg("--remount-ro")
                    .arg(path);
            }
        }
    }
    if let Some(broker) = broker {
        command
            .arg("--ro-bind")
            .arg(broker.directory())
            .arg(broker.directory());
        command
            .arg("--")
            .arg(helper)
            .arg(crate::HELPER_ARG)
            .arg("--relay-supervisor")
            .arg(broker.directory().join("broker.sock"))
            .arg(policy.proxy_max_connections.to_string())
            .arg(policy.proxy_idle_timeout_ms.to_string())
            .arg(policy.proxy_poll_interval_ms.to_string());
        for port in &broker.ports {
            command.arg(port.to_string());
        }
        for port in &broker.exposed {
            command.arg("--expose").arg(port.to_string());
        }
        command.arg("--").arg(helper).arg(crate::HELPER_ARG);
        for port in &broker.ports {
            command.arg("--isolated-tcp-port").arg(port.to_string());
        }
    } else {
        command.arg("--").arg(helper).arg(crate::HELPER_ARG);
    }
    command.arg("--namespace");
    for socket in unix_sockets {
        command.arg("--unix-socket").arg(socket);
    }
    if !network
        && policy.allow_local_binding
        && policy.mode == keke_config_types::SandboxMode::WorkspaceWrite
    {
        command.arg("--isolated-tcp-binding");
    }
    if network {
        command.arg("--network");
    }
    for root in roots {
        command.arg("--write").arg(&root.path);
    }
    command.arg("--").arg(program).args(args);
    command
}
