use keke_protocol::ContentBlock;
use keke_tasks::BackgroundTasks;
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
use std::sync::Arc;
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
    /// ten. Ignored when `background` is set — a background command has no
    /// budget, because nothing is waiting on it.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Start it and return at once, with a task id instead of output. Use for
    /// anything long-lived: a dev server, a watch, a build you want to check
    /// back on.
    #[serde(default)]
    pub background: bool,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum BashOutput {
    /// Ran to completion, which is what the model gets unless it asked
    /// otherwise.
    Finished {
        exit_code: i32,
        /// Interleaved stdout then stderr, already capped.
        output: String,
        truncated: bool,
    },
    /// Started and left running. The id is how every later call names it.
    Started { task_id: String },
}

impl ToolOutput for BashOutput {
    fn render(&self) -> Vec<ContentBlock> {
        let (exit_code, output) = match self {
            Self::Started { task_id } => {
                return vec![ContentBlock::text(format!(
                    "started {task_id} in the background — read it with `task_output`, stop it \
                     with `kill_task`"
                ))];
            }
            Self::Finished {
                exit_code, output, ..
            } => (*exit_code, output),
        };
        let mut text = output.clone();
        if text.is_empty() {
            text.push_str("(no output)");
        }
        if exit_code != 0 {
            text.push_str(&format!("\n[exit {exit_code}]"));
        }
        vec![ContentBlock::text(text)]
    }
}

/// Runs a shell command in the workspace root.
///
/// The background half is delegated rather than implemented here: a task that
/// outlives the turn cannot be owned by the call that started it, and
/// `keke-tasks` is the one place that records what a task is doing.
pub struct Bash {
    /// Where a backgrounded command goes. `None` in a composition with no task
    /// registry, which makes `background: true` an error rather than a silent
    /// foreground run — the model asked not to wait, and quietly waiting is a
    /// different answer to a different question (`AGENTS.md` invariant 8).
    pub background: Option<Arc<BackgroundTasks>>,
}

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
             `head` when you expect a lot. Set `background` for anything long-lived — a dev \
             server, a watch, a long build — to get a task id back immediately instead of \
             blocking the turn.",
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
        if args.background {
            let Some(tasks) = self.background.as_ref() else {
                return Err(ToolError::custom(
                    "background_unavailable",
                    "this session has no background task registry",
                ));
            };
            let id = tasks
                .spawn(args.command, &ctx.workspace_root)
                .map_err(|error| ToolError::custom("background_refused", error.to_string()))?;
            return Ok(BashOutput::Started { task_id: id });
        }

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

        Ok(BashOutput::Finished {
            exit_code: output.status.code().unwrap_or(-1),
            output: text,
            truncated,
        })
    }
}
