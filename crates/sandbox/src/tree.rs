//! Reclaiming everything a confined command started, not just its shell.
//!
//! `Child::kill` reaches only the process it spawned. A dev server launched as
//! `npm run dev` or `cmd &` is a grandchild of that shell, so killing the shell
//! on cancel, timeout or exit left it running — still holding its port and, for
//! a background task, still writing into a registry nobody read any more.
//!
//! [`Sandbox::command`](crate::Sandbox::command) therefore makes every command
//! the leader of its own process group, and [`ProcessTree`] signals that group.

use std::process::Command;
use std::process::Stdio;

/// The process group led by a command spawned from a [`crate::Sandbox`].
///
/// Dropping it kills the whole group, so holding one for as long as the command
/// should live is enough: an abandoned wait, a cancelled turn and a shutting
/// down runtime all drop it.
///
/// Group members that called `setsid` or `setpgid` have left the group and are
/// out of reach. On Linux the namespace backend covers that case itself, since
/// its PID namespace dies with the launcher.
#[derive(Debug)]
pub struct ProcessTree {
    leader: Option<u32>,
}

impl ProcessTree {
    /// The group led by `leader`, the pid of a child spawned from
    /// [`crate::Sandbox::command`] or [`crate::Sandbox::shell`]. `None`, which
    /// is what a child that has already been reaped reports, signals nothing.
    #[must_use]
    pub fn of(leader: Option<u32>) -> Self {
        Self {
            leader: leader.filter(|_| cfg!(unix)),
        }
    }

    /// SIGTERM to every member, so a server gets the chance to clean up.
    pub fn terminate(&self) {
        self.signal("-TERM");
    }

    /// SIGKILL to every member.
    pub fn kill(&self) {
        self.signal("-KILL");
    }

    /// The signal goes through `kill(1)` rather than `libc::kill`, because the
    /// workspace denies `unsafe`. The leading `--` and the negated pid are what
    /// make it address the group rather than one process.
    ///
    /// A group left empty gives `kill` nothing to do. The one hazard is the
    /// leader's pid being reused as a new group id after the group is gone,
    /// which needs a fresh process to call `setpgid` onto exactly that number
    /// within the window between the last member exiting and this call.
    fn signal(&self, name: &str) {
        let Some(leader) = self.leader else { return };
        let _ = Command::new("kill")
            .arg(name)
            .arg("--")
            .arg(format!("-{leader}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use std::io::BufRead;
    use std::io::BufReader;
    use std::time::Duration;
    use std::time::Instant;

    use keke_config_types::SandboxMode;
    use keke_config_types::SandboxPolicy;
    use keke_paths::AbsPath;

    use super::*;
    use crate::Sandbox;

    /// A zombie is dead for this purpose: PID 1 in a container may never reap
    /// the orphans a killed group leaves behind.
    fn running(pid: u32) -> bool {
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            let state = stat
                .rsplit(')')
                .next()
                .and_then(|rest| rest.trim().chars().next());
            return state != Some('Z');
        }
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    /// Spawns `sh -c 'sleep 60 & echo $!; wait'` and returns the shell, its
    /// background grandchild's pid, and the tree.
    fn shell_with_background_child() -> (std::process::Child, u32, ProcessTree) {
        let dir = tempfile::tempdir().expect("directory");
        let sandbox = Sandbox::new(
            SandboxPolicy {
                mode: SandboxMode::DangerFullAccess,
                ..Default::default()
            },
            None,
        )
        .expect("sandbox");
        let root = AbsPath::new(dir.path()).expect("absolute");
        let mut child = sandbox
            .shell("sleep 60 & echo $!; wait", &root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn");
        let tree = ProcessTree::of(Some(child.id()));
        let mut line = String::new();
        BufReader::new(child.stdout.take().expect("stdout"))
            .read_line(&mut line)
            .expect("grandchild pid");
        let grandchild = line.trim().parse().expect("pid");
        assert!(running(grandchild));
        (child, grandchild, tree)
    }

    fn wait_until_gone(pid: u32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if !running(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn killing_the_tree_stops_background_children_the_shell_started() {
        let (mut child, grandchild, tree) = shell_with_background_child();
        tree.kill();
        let _ = child.wait();
        assert!(wait_until_gone(grandchild));
    }

    #[test]
    fn dropping_the_tree_stops_what_an_abandoned_wait_left_running() {
        let (mut child, grandchild, tree) = shell_with_background_child();
        drop(tree);
        let _ = child.wait();
        assert!(wait_until_gone(grandchild));
    }

    #[test]
    fn killing_only_the_shell_leaves_its_children_which_is_why_the_tree_exists() {
        let (mut child, grandchild, tree) = shell_with_background_child();
        child.kill().expect("kill shell");
        let _ = child.wait();
        assert!(running(grandchild), "the premise of ProcessTree");
        drop(tree);
        assert!(wait_until_gone(grandchild));
    }
}
