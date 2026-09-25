//! macOS: run the command under `sandbox-exec` with a generated profile.
//!
//! The profile is codex's base policy — deny by default, then the handful of
//! sysctls, Mach services, and devices ordinary tools need — plus one line
//! granting reads everywhere and one granting writes under each root. Roots
//! travel as `-D` parameters rather than being spliced into the profile text,
//! so a path containing a quote cannot change what the profile says.

use std::ffi::OsStr;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Command;

/// Absolute, never looked up on `PATH`: a `sandbox-exec` earlier on `PATH`
/// would be choosing its own policy.
pub(crate) const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

const BASE_POLICY: &str = include_str!("ported/codex/seatbelt_base_policy.sbpl");
const NETWORK_POLICY: &str = include_str!("ported/codex/seatbelt_network_policy.sbpl");

pub(crate) fn command<I, S>(roots: &[PathBuf], network: bool, program: &str, args: I) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new(SANDBOX_EXEC);
    command.arg("-p").arg(profile(roots.len(), network));
    for (index, root) in roots.iter().enumerate() {
        let mut define = OsString::from(format!("-D{}=", root_param(index)));
        define.push(root);
        command.arg(define);
    }
    command.arg("--").arg(program).args(args);
    command
}

fn root_param(index: usize) -> String {
    format!("WRITABLE_ROOT_{index}")
}

fn profile(roots: usize, network: bool) -> String {
    let mut profile = String::from(BASE_POLICY);
    profile.push_str("\n(allow file-read*)\n");
    if roots > 0 {
        profile.push_str("(allow file-write*");
        for index in 0..roots {
            let _ = write!(profile, " (subpath (param \"{}\"))", root_param(index));
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

    #[test]
    fn roots_are_parameters_not_profile_text() {
        let roots = [PathBuf::from("/tmp/it's \"quoted\"")];
        let command = command(&roots, false, "true", std::iter::empty::<&str>());
        let args: Vec<_> = command.get_args().collect();
        let profile = args[1].to_string_lossy();
        assert!(
            !profile.contains("quoted"),
            "the path leaked into the profile"
        );
        assert!(profile.contains("(subpath (param \"WRITABLE_ROOT_0\"))"));
        assert_eq!(args[2], "-DWRITABLE_ROOT_0=/tmp/it's \"quoted\"");
    }

    #[test]
    fn no_network_means_no_network_rule() {
        assert!(!profile(1, false).contains("network-outbound"));
        assert!(profile(1, true).contains("(allow network-outbound)"));
    }

    #[test]
    fn no_roots_means_no_write_rule() {
        assert!(!profile(0, false).contains("(allow file-write* (subpath"));
    }
}
