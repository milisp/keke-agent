//! Linux: Landlock for the filesystem, seccomp for the network.
//!
//! Both are installed by keke's own binary re-executed as a launcher (see
//! [`crate::HELPER_ARG`]), which then execs the command. Both survive `exec`
//! and are inherited by every child, and neither can be lifted afterwards —
//! a command cannot run its way out of them.
//!
//! Landlock rather than bubblewrap: bubblewrap needs unprivileged user
//! namespaces, which Ubuntu 24.04's default AppArmor profile refuses, so the
//! sandbox would fail on exactly the machines most people run it on.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::Command;

use keke_config_types::SandboxMode;
use keke_config_types::SandboxPolicy;
use landlock::ABI;
use landlock::Access as _;
use landlock::AccessFs;
use landlock::AccessNet;
use landlock::CompatLevel;
use landlock::Compatible as _;
use landlock::NetPort;
use landlock::Ruleset;
use landlock::RulesetAttr as _;
use landlock::RulesetCreatedAttr as _;
use landlock::RulesetError;
use landlock::RulesetStatus;
use landlock::Scope;
use landlock::path_beneath_rules;
use landlock::{PathBeneath, PathFd};
use seccompiler::BpfProgram;
use seccompiler::SeccompAction;
use seccompiler::SeccompCmpArgLen;
use seccompiler::SeccompCmpOp;
use seccompiler::SeccompCondition;
use seccompiler::SeccompFilter;
use seccompiler::SeccompRule;
use seccompiler::TargetArch;

/// The newest filesystem ABI asked for. Best-effort below it: an older kernel
/// still confines writes, and only loses the finer rights (truncate, device
/// ioctls) it has no way to express.
const ABI_WANTED: ABI = ABI::V5;

/// Whether this kernel can enforce `policy`, asked once at startup so a
/// missing feature is a clear error before the first command rather than a
/// launcher failing on every one.
pub(crate) fn probe(policy: &SandboxPolicy) -> Result<(), String> {
    if policy.mode == SandboxMode::WorkspaceWrite
        && !policy.allowed_domains.is_empty()
        && policy.linux_namespace_launcher.is_none()
    {
        return Err("domain-restricted networking is not supported by the Linux launcher; refusing unrestricted fallback".to_string());
    }
    if !policy.deny_read.is_empty() && policy.linux_namespace_launcher.is_none() {
        return Err("Landlock cannot enforce deny_read: its root read grant cannot exclude credential paths".to_string());
    }
    Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(ABI::V1))
        .and_then(|ruleset| ruleset.create())
        .map_err(|error| {
            format!(
                "Landlock is unavailable ({error}); it needs Linux 5.13 or later with \
                 CONFIG_SECURITY_LANDLOCK and `landlock` in the kernel's lsm= list"
            )
        })?;
    let wants_network_filter = policy.mode == SandboxMode::ReadOnly || !policy.network_access;
    if wants_network_filter {
        target_arch()?;
    }
    Ok(())
}

/// Whether this kernel can enforce exact Unix socket grants (Landlock ABI 9).
/// Creates a throwaway ruleset without restricting this process.
pub(crate) fn probe_unix_socket_grants() -> Result<(), String> {
    Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::ResolveUnix)
        .and_then(|ruleset| ruleset.scope(Scope::AbstractUnixSocket))
        .and_then(|ruleset| ruleset.create())
        .map(|_| ())
        .map_err(|error| {
            format!("exact Unix socket grants require Landlock ResolveUnix enforcement: {error}")
        })
}

fn target_arch() -> Result<TargetArch, String> {
    TargetArch::try_from(std::env::consts::ARCH).map_err(|_| {
        format!(
            "the network filter has no seccomp definition for {}",
            std::env::consts::ARCH
        )
    })
}

struct Request {
    network: bool,
    namespace: bool,
    isolated_tcp_ports: Vec<u16>,
    isolated_tcp_binding: bool,
    unix_sockets: Vec<PathBuf>,
    writable: Vec<PathBuf>,
    program: OsString,
    args: Vec<OsString>,
}

fn parse(args: Vec<OsString>) -> Result<Request, String> {
    let mut network = false;
    let mut namespace = false;
    let mut isolated_tcp_ports = Vec::new();
    let mut isolated_tcp_binding = false;
    let mut unix_sockets = Vec::new();
    let mut writable = Vec::new();
    let mut rest = args.into_iter();
    loop {
        let Some(arg) = rest.next() else {
            return Err("no command after the launcher's options".to_string());
        };
        match arg.to_str() {
            Some("--unix-socket") => {
                let path = PathBuf::from(rest.next().ok_or("--unix-socket needs a path")?);
                if !path.is_absolute() {
                    return Err("Unix socket path must be absolute".into());
                }
                unix_sockets.push(path);
            }
            Some("--network") => network = true,
            Some("--namespace") => namespace = true,
            Some("--isolated-tcp-binding") => isolated_tcp_binding = true,
            Some("--isolated-tcp-port") => {
                let value = rest.next().ok_or("--isolated-tcp-port needs a port")?;
                let text = value.to_str().ok_or("invalid TCP port")?;
                if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err("invalid TCP port".to_string());
                }
                let port = text.parse::<u16>().map_err(|_| "invalid TCP port")?;
                if port == 0 {
                    return Err("isolated TCP port must be nonzero".to_string());
                }
                isolated_tcp_ports.push(port);
            }
            Some("--write") => {
                let root = rest
                    .next()
                    .ok_or_else(|| "--write needs a path".to_string())?;
                writable.push(PathBuf::from(root));
            }
            Some("--") => break,
            _ => return Err(format!("unknown launcher option {}", arg.to_string_lossy())),
        }
    }
    let program = rest
        .next()
        .ok_or_else(|| "no command after --".to_string())?;
    if network && (!isolated_tcp_ports.is_empty() || isolated_tcp_binding) {
        return Err("--network conflicts with isolated TCP options".to_string());
    }
    Ok(Request {
        network,
        namespace,
        isolated_tcp_ports,
        isolated_tcp_binding,
        unix_sockets,
        writable,
        program,
        args: rest.collect(),
    })
}

pub(crate) fn run(args: Vec<OsString>) -> std::io::Error {
    if args.as_slice() == [OsString::from("--probe-unix-sockets")] {
        return match restrict_unix_sockets(&[]) {
            Ok(()) => std::io::Error::other("unix-path-enforcement-ready"),
            Err(error) => std::io::Error::other(error),
        };
    }
    let request = match parse(args) {
        Ok(request) => request,
        Err(message) => return std::io::Error::new(std::io::ErrorKind::InvalidInput, message),
    };
    if let Err(message) = restrict_filesystem(&request.writable) {
        return std::io::Error::other(message);
    }
    let isolated = !request.isolated_tcp_ports.is_empty() || request.isolated_tcp_binding;
    if !request.unix_sockets.is_empty()
        && let Err(error) = restrict_unix_sockets(&request.unix_sockets)
    {
        return std::io::Error::other(error);
    }
    if (isolated || (!request.network && !request.unix_sockets.is_empty()))
        && let Err(message) =
            restrict_isolated_tcp(&request.isolated_tcp_ports, request.isolated_tcp_binding)
    {
        return std::io::Error::other(message);
    }
    if (request.namespace || isolated)
        && let Err(message) = restrict_namespace()
    {
        return std::io::Error::other(message);
    }
    if (!request.network || !request.unix_sockets.is_empty())
        && let Err(message) = restrict_network(isolated, !request.unix_sockets.is_empty())
    {
        return std::io::Error::other(message);
    }
    Command::new(&request.program).args(&request.args).exec()
}

/// Reads anywhere, writes only beneath `writable` and to `/dev/null`.
fn restrict_filesystem(writable: &[PathBuf]) -> Result<(), String> {
    let all = AccessFs::from_all(ABI_WANTED);
    let status = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(all)
        .and_then(|ruleset| ruleset.create())
        .and_then(|ruleset| {
            ruleset.add_rules(path_beneath_rules(["/"], AccessFs::from_read(ABI_WANTED)))
        })
        .and_then(|ruleset| ruleset.add_rules(path_beneath_rules(["/dev/null"], all)))
        .and_then(|ruleset| ruleset.add_rules(path_beneath_rules(writable, all)))
        .and_then(|ruleset| ruleset.restrict_self())
        .map_err(|error| format!("installing the Landlock ruleset failed: {error}"))?;
    if status.ruleset == RulesetStatus::NotEnforced {
        return Err("the kernel accepted the Landlock ruleset but is not enforcing it".to_string());
    }
    Ok(())
}

/// Permit Unix socket creation/socketpair for child-process IPC, but refuse
/// fresh connections/listeners even for Unix sockets: seccomp cannot inspect
/// sockaddr paths to distinguish an owned build worker from a host daemon.
///
/// The syscall list is codex's (`codex-rs/linux-sandbox/src/landlock.rs`).
/// Socketpair stays for build tools; arbitrary host Unix services do not.
fn restrict_network(isolated_tcp: bool, unix: bool) -> Result<(), String> {
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    for number in [
        libc::SYS_connect,
        libc::SYS_accept,
        libc::SYS_accept4,
        libc::SYS_bind,
        libc::SYS_listen,
        libc::SYS_getpeername,
        libc::SYS_getsockname,
        libc::SYS_shutdown,
        libc::SYS_sendto,
        libc::SYS_sendmmsg,
        libc::SYS_recvmmsg,
        libc::SYS_getsockopt,
        libc::SYS_setsockopt,
        // Not network calls, but ways around the filter: io_uring performs
        // socket operations without the syscalls above, and ptrace would let
        // a command drive an unfiltered process.
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
    ] {
        if (isolated_tcp || unix)
            && ![
                libc::SYS_io_uring_setup,
                libc::SYS_io_uring_enter,
                libc::SYS_io_uring_register,
                libc::SYS_ptrace,
                libc::SYS_process_vm_readv,
                libc::SYS_process_vm_writev,
            ]
            .contains(&number)
        {
            continue;
        }
        // No conditions: the call is refused whatever its arguments.
        rules.insert(number, Vec::new());
    }
    let not_unix = SeccompCondition::new(
        0,
        SeccompCmpArgLen::Dword,
        SeccompCmpOp::Ne,
        libc::AF_UNIX as u64,
    )
    .and_then(|condition| SeccompRule::new(vec![condition]))
    .map_err(|error| error.to_string())?;
    rules.insert(libc::SYS_socket, vec![not_unix.clone()]);
    rules.insert(libc::SYS_socketpair, vec![not_unix]);

    if isolated_tcp || unix {
        // Deny the complement of TCP creation, without dereferencing sockaddr.
        // Only the two standard socket flags may accompany SOCK_STREAM.
        let deny = |index, values: &[i32]| -> Result<SeccompRule, String> {
            let conditions = values
                .iter()
                .map(|value| {
                    SeccompCondition::new(
                        index,
                        SeccompCmpArgLen::Dword,
                        SeccompCmpOp::Ne,
                        *value as u64,
                    )
                })
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?;
            SeccompRule::new(conditions).map_err(|error| error.to_string())
        };
        rules.insert(
            libc::SYS_socket,
            vec![
                deny(
                    0,
                    &if unix {
                        vec![libc::AF_UNIX, libc::AF_INET, libc::AF_INET6]
                    } else {
                        vec![libc::AF_INET, libc::AF_INET6]
                    },
                )?,
                deny(
                    1,
                    &[
                        libc::SOCK_STREAM,
                        libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
                        libc::SOCK_STREAM | libc::SOCK_NONBLOCK,
                        libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                    ],
                )?,
                deny(2, &[0, libc::IPPROTO_TCP])?,
            ],
        );
    }
    install_filter(rules)
}

fn restrict_unix_sockets(paths: &[PathBuf]) -> Result<(), String> {
    let enforcement_error = |error: RulesetError| {
        format!("exact Unix socket grants require Landlock ResolveUnix enforcement: {error}")
    };
    // Open grants first: `PathFdError` and `RulesetError` are distinct error types.
    let grants = paths
        .iter()
        .map(|path| {
            PathFd::new(path).map_err(|error| {
                format!(
                    "opening Unix socket grant {} failed: {error}",
                    path.display()
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::ResolveUnix)
        .and_then(|ruleset| ruleset.scope(Scope::AbstractUnixSocket))
        .and_then(|ruleset| ruleset.create())
        .map_err(enforcement_error)?;
    for fd in grants {
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, AccessFs::ResolveUnix))
            .map_err(enforcement_error)?;
    }
    let status = ruleset.restrict_self().map_err(enforcement_error)?;
    if status.ruleset != RulesetStatus::FullyEnforced {
        return Err("Unix socket rules are not fully enforced".into());
    }
    Ok(())
}

/// Requires a caller-created private, loopback-only network namespace. These
/// port grants must never be used on the host network: Landlock checks ports,
/// not addresses. Unsupported network rights fail closed.
fn restrict_isolated_tcp(ports: &[u16], binding: bool) -> Result<(), String> {
    let ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessNet::ConnectTcp)
        .map_err(|error| format!("requiring Landlock TCP connect enforcement failed: {error}"))?;
    let ruleset = if binding {
        ruleset
    } else {
        ruleset
            .handle_access(AccessNet::BindTcp)
            .map_err(|error| format!("requiring Landlock TCP bind enforcement failed: {error}"))?
    };
    let status = ruleset
        .create()
        .and_then(|ruleset| {
            ruleset.add_rules(
                ports
                    .iter()
                    .map(|port| Ok(NetPort::new(*port, AccessNet::ConnectTcp))),
            )
        })
        .and_then(|ruleset| ruleset.restrict_self())
        .map_err(|error| format!("installing isolated TCP Landlock rules failed: {error}"))?;
    if status.ruleset != RulesetStatus::FullyEnforced {
        return Err("isolated TCP Landlock rules are not fully enforced".to_string());
    }
    Ok(())
}

/// Prevent escaping or reconfiguring namespaces even with unrestricted TCP.
fn restrict_namespace() -> Result<(), String> {
    install_filter(
        [
            libc::SYS_setns,
            libc::SYS_unshare,
            libc::SYS_mount,
            libc::SYS_umount2,
            libc::SYS_pivot_root,
            libc::SYS_open_by_handle_at,
            libc::SYS_bpf,
        ]
        .into_iter()
        .map(|number| (number, Vec::new()))
        .collect(),
    )
}

fn install_filter(rules: BTreeMap<i64, Vec<SeccompRule>>) -> Result<(), String> {
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        target_arch()?,
    )
    .map_err(|error| error.to_string())?;
    let program: BpfProgram = filter
        .try_into()
        .map_err(|error: seccompiler::BackendError| error.to_string())?;
    seccompiler::apply_filter(&program)
        .map_err(|error| format!("installing the network filter failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn options_end_at_the_separator() {
        let request = parse(args(&[
            "--network",
            "--write",
            "/w",
            "--",
            "sh",
            "-c",
            "--write",
        ]))
        .expect("parses");
        assert!(request.network);
        assert_eq!(request.writable, [PathBuf::from("/w")]);
        assert_eq!(request.program, "sh");
        assert_eq!(request.args, args(&["-c", "--write"]));
    }

    /// A launcher that misread its options and ran the command anyway would
    /// be running it with a policy nobody asked for.
    #[test]
    fn an_unknown_option_is_refused() {
        assert!(parse(args(&["--wirte", "/w", "--", "sh"])).is_err());
        assert!(parse(args(&["--write"])).is_err());
        assert!(parse(args(&["--"])).is_err());
    }

    #[test]
    fn isolated_options_and_independent_namespace_guard_parse() {
        let request = parse(args(&[
            "--namespace",
            "--isolated-tcp-port",
            "1234",
            "--isolated-tcp-port",
            "65535",
            "--isolated-tcp-binding",
            "--",
            "sh",
        ]))
        .expect("parses");
        assert!(request.namespace);
        assert!(request.isolated_tcp_binding);
        assert_eq!(request.isolated_tcp_ports, [1234, 65535]);
        for flags in [vec!["--namespace"], vec!["--namespace", "--network"]] {
            let mut flags = flags;
            flags.extend(["--", "sh"]);
            assert!(parse(args(&flags)).expect("parses").namespace);
        }
    }

    #[test]
    fn malformed_or_contradictory_isolated_options_are_refused() {
        for port in ["0", "65536", "-1", "+1", "", "1.0", "tcp", "--"] {
            assert!(parse(args(&["--isolated-tcp-port", port, "--", "sh"])).is_err());
        }
        assert!(parse(args(&["--isolated-tcp-port"])).is_err());
        assert!(parse(args(&["--isolated-tcp-binding", "true", "--", "sh"])).is_err());
        assert!(parse(args(&["--network", "--isolated-tcp-binding", "--", "sh"])).is_err());
        assert!(
            parse(args(&[
                "--isolated-tcp-port",
                "80",
                "--network",
                "--",
                "sh"
            ]))
            .is_err()
        );
    }
}
