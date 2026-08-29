use keke_protocol::ContentBlock;
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
use serde::Serialize;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;
use tokio::process::Command;

use crate::support;

/// Used when the model names no budget.
const DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// The ceiling the model cannot raise past. Advertised as this tool's
/// `ToolCapabilities::timeout_millis`, which is what the engine enforces, so
/// the two numbers are the same number rather than two that can drift.
const MAX_TIMEOUT_MS: u64 = 600_000;
/// How often cancellation is observed while the child runs.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BashArgs {
    /// Shell command line, run from the workspace root.
    pub command: String,
    /// Wall-clock budget in milliseconds. Defaults to two minutes, capped at
    /// ten.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct BashOutput {
    pub exit_code: i32,
    /// Interleaved stdout then stderr, already capped.
    pub output: String,
    pub truncated: bool,
}

impl ToolOutput for BashOutput {
    fn render(&self) -> Vec<ContentBlock> {
        let mut text = self.output.clone();
        if text.is_empty() {
            text.push_str("(no output)");
        }
        if self.exit_code != 0 {
            text.push_str(&format!("\n[exit {}]", self.exit_code));
        }
        vec![ContentBlock::text(text)]
    }
}

/// Runs a shell command in the workspace root.
pub struct Bash;

impl Tool for Bash {
    type Args = BashArgs;
    type Output = BashOutput;

    fn id(&self) -> ToolId {
        ToolId::new("bash")
    }

    fn description(&self, _ctx: &ListToolsContext) -> ToolDescription {
        ToolDescription::new(
            "Run a shell command from the workspace root. Returns stdout and stderr combined, \
             plus the exit code when it is non-zero. Long output is truncated, so pipe through \
             `head` when you expect a lot.",
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            approval: ApprovalRequirement::ByPolicy,
            kind: ToolKind::Execute,
            // A shell command can touch anything the other calls in the step
            // are touching, so it never runs beside a sibling.
            concurrency_safe: false,
            timeout_millis: Some(MAX_TIMEOUT_MS),
        }
    }

    async fn run(&self, ctx: ToolCallContext, args: Self::Args) -> Result<Self::Output, ToolError> {
        // Clamp to the budget the engine is enforcing rather than to a local
        // copy of it: overrunning gets the call killed from outside, losing
        // whatever output the command had produced.
        let ceiling = ctx.timeout_millis.unwrap_or(MAX_TIMEOUT_MS);
        let millis = args.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).min(ceiling);
        let deadline = Instant::now() + Duration::from_millis(millis);

        let (program, flag) = if cfg!(windows) {
            ("cmd", "/C")
        } else {
            ("sh", "-c")
        };

        let child = Command::new(program)
            .arg(flag)
            .arg(&args.command)
            .current_dir(ctx.workspace_root.as_path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Abandoning the wait future must not leave the child running: both
            // the timeout and cancellation paths drop it.
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| ToolError::custom("spawn_failed", format!("{program}: {error}")))?;

        let wait = child.wait_with_output();
        tokio::pin!(wait);

        let output = loop {
            tokio::select! {
                finished = &mut wait => {
                    break finished.map_err(|error| {
                        ToolError::custom("wait_failed", error.to_string())
                    })?;
                }
                () = tokio::time::sleep(POLL_INTERVAL) => {
                    if ctx.is_cancelled() {
                        return Err(ToolError::Cancelled);
                    }
                    if Instant::now() >= deadline {
                        return Err(ToolError::Timeout { millis });
                    }
                }
            }
        };

        let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
        if !output.stderr.is_empty() {
            if !combined.is_empty() && !combined.ends_with('\n') {
                combined.push('\n');
            }
            combined.push_str(&String::from_utf8_lossy(&output.stderr));
        }
        let (text, truncated) = support::cap(combined, "output truncated");

        Ok(BashOutput {
            exit_code: output.status.code().unwrap_or(-1),
            output: text,
            truncated,
        })
    }
}
