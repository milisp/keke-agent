use keke_config_types::SandboxMode;
use keke_protocol::ContentBlock;
use keke_sandbox::Sandbox;
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
    /// What confines the command. Held here rather than consulted per call
    /// because it was checked to be enforceable when it was built, and a
    /// sandbox that could fail to build mid-turn would need a fallback — the
    /// only one available being to run the command bare.
    pub sandbox: Arc<Sandbox>,
    /// Where a backgrounded command goes. `None` in a composition with no task
    /// registry, which makes `background: true` an error rather than a silent
    /// foreground run — the model asked not to wait, and quietly waiting is a
    /// different answer to a different question (`AGENTS.md` invariant 8).
    pub background: Option<Arc<BackgroundTasks>>,
}

/// What a confined command cannot do, in words a model can act on. `None`
/// when commands are not confined.
fn sandbox_limits(sandbox: &Sandbox) -> Option<String> {
    if !sandbox.confines() {
        return None;
    }
    let policy = sandbox.policy();
    let writes = match policy.mode {
        SandboxMode::ReadOnly => "nothing may be written",
        _ => {
            "writes are allowed only inside the workspace and the temporary directory, and \
              `.git` is read-only"
        }
    };
    let network = if policy.mode == SandboxMode::WorkspaceWrite && policy.network_access {
        ""
    } else {
        "; there is no network access"
    };
    Some(format!("{writes}{network}"))
}

impl Tool for Bash {
    type Args = BashArgs;
    type Output = BashOutput;

    fn id(&self) -> ToolId {
        ToolId::new("bash")
    }

    fn description(&self, _ctx: &ListToolsContext) -> ToolDescription {
        let mut text = String::from(
            "Run a shell command from the workspace root. Returns stdout and stderr combined, \
             plus the exit code when it is non-zero. Long output is truncated, so pipe through \
             `head` when you expect a lot. Set `background` for anything long-lived — a dev \
             server, a watch, a long build — to get a task id back immediately instead of \
             blocking the turn.",
        );
        // The model cannot tell a sandbox denial from any other failure unless
        // it knows the sandbox is there, and would otherwise retry the same
        // command or give up on a task a person would happily approve.
        if let Some(limits) = sandbox_limits(&self.sandbox) {
            text.push_str(&format!(
                "\n\nCommands run in a sandbox: {limits}. When a command fails because of \
                 that — \"Operation not permitted\", \"Permission denied\", or a network error \
                 — and it genuinely needs more, rerun it with `bash_unsandboxed`, which asks \
                 the person first."
            ));
        }
        ToolDescription::new(text)
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            // Where the configured sandbox does not exist, a person stands in
            // for it on every command — including the ones a standing "allow
            // always" or a permissive policy would have waved through, since
            // both assumed the command would be confined.
            approval: if !self.sandbox.is_enforced() {
                ApprovalRequirement::Always
            } else if self.sandbox.policy().mode == SandboxMode::WorkspaceWrite
                && self.sandbox.policy().auto_approve_bash
            {
                ApprovalRequirement::AutoApproved
            } else {
                ApprovalRequirement::ByPolicy
            },
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

        let child = Command::from(self.sandbox.shell(&args.command, &ctx.workspace_root))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Abandoning the wait future must not leave the child running: both
            // the timeout and cancellation paths drop it.
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| ToolError::custom("spawn_failed", error.to_string()))?;

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
