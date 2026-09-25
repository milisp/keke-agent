//! Scoped Git writes for the edit-accepting policy.
//!
//! A shell line cannot safely be recognized as "just git add" or "just git
//! commit": it can contain expansions, aliases, and another command. These
//! tools build Git's argument vector directly and keep Git inside the OS
//! sandbox while allowing the repository metadata that those two operations
//! must update. Hooks are disabled so repository content is not executed by
//! accepting an edit.

use std::process::Stdio;
use std::sync::Arc;

use keke_config_types::SandboxMode;
use keke_protocol::ContentBlock;
use keke_sandbox::Sandbox;
use keke_tool::ApprovalRequirement;
use keke_tool::ListToolsContext;
use keke_tool::Tool;
use keke_tool::ToolCallContext;
use keke_tool::ToolCapabilities;
use keke_tool::ToolDescription;
use keke_tool::ToolError;
use keke_tool::ToolId;
use keke_tool::ToolKind;
use keke_tool::ToolOutput;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::process::Command;

use crate::support;

const GIT_TIMEOUT_MS: u64 = 120_000;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GitAddArgs {
    /// Paths to stage. Empty stages all changes in the workspace.
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GitCommitArgs {
    /// Commit message.
    pub message: String,
}

#[derive(Debug, serde::Serialize)]
pub struct GitOutput {
    pub exit_code: i32,
    pub output: String,
}

impl ToolOutput for GitOutput {
    fn render(&self) -> Vec<ContentBlock> {
        let mut output = self.output.clone();
        if output.is_empty() {
            output.push_str("(no output)");
        }
        if self.exit_code != 0 {
            output.push_str(&format!("\n[exit {}]", self.exit_code));
        }
        vec![ContentBlock::text(output)]
    }
}

pub struct GitAdd {
    pub sandbox: Arc<Sandbox>,
}

pub struct GitCommit {
    pub sandbox: Arc<Sandbox>,
}

fn capabilities(sandbox: &Sandbox) -> ToolCapabilities {
    ToolCapabilities {
        kind: ToolKind::Edit,
        approval: if sandbox.is_enforced() && sandbox.policy().mode == SandboxMode::WorkspaceWrite {
            ApprovalRequirement::ByPolicy
        } else {
            ApprovalRequirement::Always
        },
        concurrency_safe: false,
        timeout_millis: Some(GIT_TIMEOUT_MS),
    }
}

async fn run_git(
    sandbox: &Sandbox,
    ctx: &ToolCallContext,
    args: &[String],
) -> Result<GitOutput, ToolError> {
    let mut command = Command::from(sandbox.git_command(args, &ctx.workspace_root));
    let output = command
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| ToolError::custom("git_failed", error.to_string()))?;
    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.stderr.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    let exit_code = output.status.code().unwrap_or(-1);
    let (output, _) = support::cap(combined, "output truncated");
    Ok(GitOutput { exit_code, output })
}

impl Tool for GitAdd {
    type Args = GitAddArgs;
    type Output = GitOutput;

    fn id(&self) -> ToolId {
        ToolId::new("git_add")
    }

    fn description(&self, _ctx: &ListToolsContext) -> ToolDescription {
        ToolDescription::new(
            "Stage workspace changes in Git. This direct Git operation runs inside the sandbox \
             with only repository metadata made writable; it does not execute Git hooks. \
             Use this instead of shell `git add`.",
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        capabilities(&self.sandbox)
    }

    async fn run(&self, ctx: ToolCallContext, args: Self::Args) -> Result<Self::Output, ToolError> {
        let mut command = vec![
            "-c".to_string(),
            "core.hooksPath=/dev/null".to_string(),
            "add".to_string(),
            "-A".to_string(),
            "--".to_string(),
        ];
        if args.paths.is_empty() {
            command.push(".".to_string());
        } else {
            command.extend(args.paths);
        }
        run_git(&self.sandbox, &ctx, &command).await
    }
}

impl Tool for GitCommit {
    type Args = GitCommitArgs;
    type Output = GitOutput;

    fn id(&self) -> ToolId {
        ToolId::new("git_commit")
    }

    fn description(&self, _ctx: &ListToolsContext) -> ToolDescription {
        ToolDescription::new(
            "Commit staged Git changes with a message. Runs inside the sandbox with Git hooks \
             and signing disabled. Use this instead of shell `git commit`.",
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        capabilities(&self.sandbox)
    }

    async fn run(&self, ctx: ToolCallContext, args: Self::Args) -> Result<Self::Output, ToolError> {
        let command = [
            "-c".to_string(),
            "core.hooksPath=/dev/null".to_string(),
            "-c".to_string(),
            "commit.gpgsign=false".to_string(),
            "commit".to_string(),
            "-m".to_string(),
            args.message,
        ];
        run_git(&self.sandbox, &ctx, &command).await
    }
}

#[cfg(all(test, target_os = "macos"))]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use keke_paths::AbsPath;
    use keke_protocol::ToolCallId;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command as StdCommand;

    fn context(root: &std::path::Path) -> ToolCallContext {
        ToolCallContext {
            call_id: ToolCallId::new("git-test"),
            workspace_root: AbsPath::new(root.canonicalize().expect("canonical root"))
                .expect("absolute root"),
            timeout_millis: Some(GIT_TIMEOUT_MS),
            cancelled: Arc::new(|| false),
        }
    }

    fn git(root: &std::path::Path, args: &[&str]) {
        let output = StdCommand::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .expect("git starts");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[tokio::test]
    async fn staged_edits_and_commit_run_inside_the_sandbox() {
        let root = tempfile::tempdir().expect("workspace");
        git(root.path(), &["init", "-q"]);
        git(root.path(), &["config", "user.name", "Keke Test"]);
        git(root.path(), &["config", "user.email", "keke@example.test"]);
        std::fs::write(root.path().join("change.txt"), "hello\n").expect("write");
        let hook = root.path().join(".git/hooks/pre-commit");
        std::fs::write(&hook, "#!/bin/sh\ntouch hook-ran\n").expect("hook");
        let mut permissions = std::fs::metadata(&hook).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&hook, permissions).expect("hook mode");

        let sandbox = Arc::new(
            Sandbox::new(
                keke_config_types::SandboxPolicy::default(),
                Some(std::path::PathBuf::from("/proc/self/exe")),
            )
            .expect("sandbox"),
        );
        let staged = GitAdd {
            sandbox: Arc::clone(&sandbox),
        }
        .run(context(root.path()), GitAddArgs { paths: vec![] })
        .await
        .expect("stage runs");
        assert_eq!(staged.exit_code, 0, "{}", staged.output);

        let committed = GitCommit { sandbox }
            .run(
                context(root.path()),
                GitCommitArgs {
                    message: "Record change".to_string(),
                },
            )
            .await
            .expect("commit runs");
        assert_eq!(committed.exit_code, 0, "{}", committed.output);
        assert!(!root.path().join("hook-ran").exists(), "hook must not run");
        git(root.path(), &["log", "-1", "--format=%s"]);
    }
}
