//! Process execution with a timeout.
//!
//! The engine uses this for its own subprocesses (VCS queries, hook commands).
//! Tools that run shell commands own their own execution so they can stream
//! progress; this is the simple, buffered case.

use std::process::Stdio;
use std::time::Duration;

use keke_paths::AbsPath;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::WorkspaceError;

/// What a finished command produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandOutcome {
    /// `None` when the process was terminated by a signal.
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutcome {
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// Run `program` with `args` in `cwd`, killing it after `timeout`.
///
/// A timeout is reported as [`WorkspaceError::Timeout`] rather than as a failed
/// command, because "it took too long" and "it exited non-zero" call for
/// different responses and collapsing them loses that.
pub async fn run_command(
    program: &str,
    args: &[&str],
    cwd: &AbsPath,
    timeout: Duration,
) -> Result<CommandOutcome, WorkspaceError> {
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd.as_path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| WorkspaceError::Io {
            path: program.to_string(),
            source,
        })?;

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let mut stdout = String::new();
    let mut stderr = String::new();

    let collect = async {
        if let Some(pipe) = stdout_pipe.as_mut() {
            let _ = pipe.read_to_string(&mut stdout).await;
        }
        if let Some(pipe) = stderr_pipe.as_mut() {
            let _ = pipe.read_to_string(&mut stderr).await;
        }
        child.wait().await
    };

    match tokio::time::timeout(timeout, collect).await {
        Ok(Ok(status)) => Ok(CommandOutcome {
            exit_code: status.code(),
            stdout,
            stderr,
        }),
        Ok(Err(source)) => Err(WorkspaceError::Io {
            path: program.to_string(),
            source,
        }),
        // `kill_on_drop` above means the child dies with the dropped future.
        Err(_) => Err(WorkspaceError::Timeout {
            millis: timeout.as_millis() as u64,
        }),
    }
}
