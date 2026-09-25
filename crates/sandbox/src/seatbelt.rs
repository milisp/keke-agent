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

use crate::WritableRoot;

/// Absolute, never looked up on `PATH`: a `sandbox-exec` earlier on `PATH`
/// would be choosing its own policy.
pub(crate) const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

const BASE_POLICY: &str = include_str!("ported/codex/seatbelt_base_policy.sbpl");
const NETWORK_POLICY: &str = include_str!("ported/codex/seatbelt_network_policy.sbpl");

pub(crate) fn command<I, S>(
    roots: &[WritableRoot],
    network: bool,
    program: &str,
    args: I,
) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let protected = protected(roots);
    let mut command = Command::new(SANDBOX_EXEC);
    command
        .arg("-p")
        .arg(profile(roots.len(), protected.len(), network));
    for (index, root) in roots.iter().enumerate() {
        command.arg(define(&root_param(index), &root.path));
    }
    for (index, path) in protected.iter().enumerate() {
        command.arg(define(&protected_param(index), path));
    }
    command.arg("--").arg(program).args(args);
    command
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
        let command = command(&roots, false, "true", std::iter::empty::<&str>());
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
    fn no_roots_means_no_write_rule() {
        assert!(!profile(0, 0, false).contains("(allow file-write*\n"));
    }
}
