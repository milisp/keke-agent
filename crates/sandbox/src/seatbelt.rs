//! macOS: run the command under `sandbox-exec` with a generated profile.
//!
//! The profile is codex's base policy — deny by default, then the handful of
//! sysctls, Mach services, and devices ordinary tools need — plus one line
//! granting reads everywhere and one granting writes under each root, less its
//! protected metadata. Paths travel as `-D` parameters rather than being
//! spliced into the profile text, so a path containing a quote cannot change
//! what the profile says.

use std::ffi::OsStr;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::process::Command;

use keke_paths::AbsPath;

use crate::WritableRoot;

/// Absolute, never looked up on `PATH`: a `sandbox-exec` earlier on `PATH`
/// would be choosing its own policy.
pub(crate) const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

const BASE_POLICY: &str = include_str!("ported/codex/seatbelt_base_policy.sbpl");
const NETWORK_POLICY: &str = include_str!("ported/codex/seatbelt_network_policy.sbpl");

pub(crate) fn command<I, S>(
    roots: &[WritableRoot],
    network: bool,
    proxy_port: Option<u16>,
    allow_local_binding: bool,
    deny_read: &[AbsPath],
    program: &str,
    args: I,
) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let protected = protected(roots);
    let denied = denied_paths(deny_read);
    let mut policy = profile(roots.len(), protected.len(), network);
    if allow_local_binding && !network {
        // A listener needs both bind and inbound rules. No outbound grant
        // accompanies this server right; localhost clients still need consent.
        policy.push_str("\n(allow network-bind (local tcp \"localhost:*\"))\n(allow network-inbound (local tcp \"localhost:*\"))\n");
    }
    if let Some(port) = proxy_port {
        let _ = writeln!(
            policy,
            "(allow network-outbound (remote tcp \"localhost:{port}\"))"
        );
    }
    append_read_denials(&mut policy, denied.len());
    let mut command = Command::new(SANDBOX_EXEC);
    command.arg("-p").arg(policy);
    for (index, root) in roots.iter().enumerate() {
        command.arg(define(&root_param(index), &root.path));
    }
    for (index, path) in protected.iter().enumerate() {
        command.arg(define(&protected_param(index), path));
    }
    for (index, path) in denied.iter().enumerate() {
        command.arg(define(&deny_read_param(index), path));
    }
    command.arg("--").arg(program).args(args);
    command
}

fn deny_read_param(index: usize) -> String {
    format!("DENY_READ_{index}")
}

fn denied_paths(paths: &[AbsPath]) -> Vec<std::path::PathBuf> {
    let mut denied = Vec::new();
    for path in paths {
        // Keep the configured spelling too: a currently absent path or a
        // symlink replaced later must not lose its restriction.
        denied.push(path.as_path().to_path_buf());
        if let Ok(canonical) = path.as_path().canonicalize()
            && canonical != path.as_path()
        {
            denied.push(canonical);
        }
    }
    denied
}

fn append_read_denials(profile: &mut String, count: usize) {
    for index in 0..count {
        let parameter = deny_read_param(index);
        let _ = writeln!(
            profile,
            "(deny file-read* (literal (param \"{parameter}\")) (subpath (param \"{parameter}\")))"
        );
    }
}

/// Every root's protected paths, excluded from *every* root's grant.
///
/// Seatbelt unions its allow rules, so a carve-out on the root a path belongs
/// to is undone by any other root that contains it — a workspace under `/tmp`,
/// or one inside a configured writable root, would otherwise have a writable
/// `.git` through the outer grant.
fn protected(roots: &[WritableRoot]) -> Vec<&std::path::Path> {
    let mut protected: Vec<&std::path::Path> = Vec::new();
    for path in roots.iter().flat_map(|root| &root.read_only) {
        if !protected.contains(&path.as_path()) {
            protected.push(path);
        }
    }
    protected
}

fn define(name: &str, path: &std::path::Path) -> OsString {
    let mut define = OsString::from(format!("-D{name}="));
    define.push(path);
    define
}

fn root_param(index: usize) -> String {
    format!("WRITABLE_ROOT_{index}")
}

fn protected_param(index: usize) -> String {
    format!("READ_ONLY_{index}")
}

fn profile(roots: usize, protected: usize, network: bool) -> String {
    let mut profile = String::from(BASE_POLICY);
    profile.push_str("\n(allow file-read*)\n");
    if roots > 0 {
        let mut exclusions = String::new();
        for index in 0..protected {
            let _ = write!(
                exclusions,
                " (require-not (subpath (param \"{}\")))",
                protected_param(index)
            );
        }
        profile.push_str("(allow file-write*");
        for index in 0..roots {
            let _ = write!(
                profile,
                "\n  (require-all (subpath (param \"{}\")){exclusions})",
                root_param(index)
            );
        }
        profile.push_str(")\n");
    }
    if network {
        profile.push_str(NETWORK_POLICY);
        profile.push_str("\n(allow network-outbound)\n(allow network-inbound)\n");
    }
    profile
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn root(path: &str, read_only: &[&str]) -> WritableRoot {
        WritableRoot {
            path: PathBuf::from(path),
            read_only: read_only.iter().map(PathBuf::from).collect(),
        }
    }

    #[test]
    fn paths_are_parameters_not_profile_text() {
        let roots = [root("/tmp/it's \"quoted\"", &["/tmp/it's \"quoted\"/.git"])];
        let command = command(
            &roots,
            false,
            None,
            false,
            &[],
            "true",
            std::iter::empty::<&str>(),
        );
        let args: Vec<_> = command.get_args().collect();
        let profile = args[1].to_string_lossy();
        assert!(
            !profile.contains("quoted"),
            "a path leaked into the profile"
        );
        assert!(profile.contains("(require-all (subpath (param \"WRITABLE_ROOT_0\"))"));
        assert!(profile.contains("(require-not (subpath (param \"READ_ONLY_0\")))"));
        assert_eq!(args[2], "-DWRITABLE_ROOT_0=/tmp/it's \"quoted\"");
        assert_eq!(args[3], "-DREAD_ONLY_0=/tmp/it's \"quoted\"/.git");
    }

    /// Allow rules union, so a path protected under one root must be excluded
    /// from the others too, or an outer root grants it back.
    #[test]
    fn a_protected_path_is_excluded_from_every_root() {
        let roots = [root("/tmp", &[]), root("/tmp/work", &["/tmp/work/.git"])];
        let text = profile(roots.len(), protected(&roots).len(), false);
        assert_eq!(
            text.matches("(require-not (subpath (param \"READ_ONLY_0\")))")
                .count(),
            2,
            "{text}"
        );
    }

    #[test]
    fn no_network_means_no_network_rule() {
        let roots = [root("/w", &[])];
        assert!(!profile(roots.len(), 0, false).contains("network-outbound"));
        assert!(profile(roots.len(), 0, true).contains("(allow network-outbound)"));
    }

    #[test]
    fn local_server_permission_is_not_an_outbound_network_grant() {
        let command = command(
            &[],
            false,
            None,
            true,
            &[],
            "true",
            std::iter::empty::<&str>(),
        );
        let text = command.get_args().nth(1).unwrap().to_string_lossy();
        assert!(text.contains("(allow network-bind (local tcp \"localhost:*\"))"));
        assert!(text.contains("(allow network-inbound (local tcp \"localhost:*\"))"));
        assert!(!text.contains("network-outbound"));
        assert!(!text.contains("\"*:*\""));
    }

    #[test]
    fn no_roots_means_no_write_rule() {
        assert!(!profile(0, 0, false).contains("(allow file-write*\n"));
    }

    #[test]
    fn read_denials_use_parameters_for_literals_and_descendants() {
        let mut text = profile(0, 0, false);
        append_read_denials(&mut text, 1);
        assert!(text.contains(
            "(deny file-read* (literal (param \"DENY_READ_0\")) (subpath (param \"DENY_READ_0\")))"
        ));
        let path = AbsPath::new("/nonexistent/credential \"quoted\"").unwrap();
        let paths = denied_paths(std::slice::from_ref(&path));
        assert_eq!(paths, vec![path.as_path().to_path_buf()]);
        assert_eq!(
            define(&deny_read_param(0), &paths[0]),
            "-DDENY_READ_0=/nonexistent/credential \"quoted\""
        );
        assert!(!text.contains("quoted"));
    }

    #[test]
    fn read_denials_cover_symlink_spelling_and_existing_target() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("credential");
        std::fs::write(&target, "secret").unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let paths = denied_paths(&[AbsPath::new(&link).unwrap()]);
        assert!(paths.contains(&link));
        assert!(paths.contains(&target.canonicalize().unwrap()));
    }
}
