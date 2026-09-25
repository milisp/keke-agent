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
use landlock::CompatLevel;
use landlock::Compatible as _;
use landlock::Ruleset;
use landlock::RulesetAttr as _;
use landlock::RulesetCreatedAttr as _;
use landlock::RulesetStatus;
use landlock::path_beneath_rules;
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
    writable: Vec<PathBuf>,
    program: OsString,
    args: Vec<OsString>,
}

fn parse(args: Vec<OsString>) -> Result<Request, String> {
    let mut network = false;
    let mut writable = Vec::new();
    let mut rest = args.into_iter();
    loop {
        let Some(arg) = rest.next() else {
            return Err("no command after the launcher's options".to_string());
        };
        match arg.to_str() {
            Some("--network") => network = true,
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
    Ok(Request {
        network,
        writable,
        program,
        args: rest.collect(),
    })
}

pub(crate) fn run(args: Vec<OsString>) -> std::io::Error {
    let request = match parse(args) {
        Ok(request) => request,
        Err(message) => return std::io::Error::new(std::io::ErrorKind::InvalidInput, message),
    };
    if let Err(message) = restrict_filesystem(&request.writable) {
        return std::io::Error::other(message);
    }
    if !request.network
        && let Err(message) = restrict_network()
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

/// Refuse every socket but a Unix one, and the calls that would use one.
///
/// The syscall list is codex's (`codex-rs/linux-sandbox/src/landlock.rs`).
/// Unix sockets stay: build tools talk to their own children over
/// `socketpair`, and a Unix socket reaches this machine, not the network.
fn restrict_network() -> Result<(), String> {
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
}
