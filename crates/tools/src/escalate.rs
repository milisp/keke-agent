//! Commands outside the sandbox require review on every call.
//!
//! Auto delegates to the guardian; human policies ask a person. Never skips
//! execution approval; standing permission does not skip a scoped review.

use std::sync::Arc;

use keke_tool::ApprovalRequirement;
use keke_tool::ListToolsContext;
use keke_tool::Tool;
use keke_tool::ToolCallContext;
use keke_tool::ToolCapabilities;
use keke_tool::ToolDescription;
use keke_tool::ToolError;
use keke_tool::ToolId;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::bash::Bash;
use crate::bash::BashArgs;
use crate::bash::BashOutput;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BashUnsandboxedArgs {
    /// Shell command line, run from the workspace root with no sandbox.
    pub command: String,
    /// Why this cannot run in the sandbox, for the reviewer deciding — what it
    /// needs (the network, a path outside the workspace, `.git`) and why.
    pub justification: String,
    /// Wall-clock budget in milliseconds, as for `bash`.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Keep the reviewed command running as a tracked background task.
    /// Approval includes its unconfined lifetime until exit or kill_task.
    #[serde(default)]
    pub background: bool,
}

/// `bash` without the sandbox, asked about every time.
///
/// Offered only where the sandbox confines something; with nothing confined
/// there is nothing to step outside of. A background escape is explicit in
/// the reviewed arguments and remains owned by the task registry.
pub struct BashUnsandboxed {
    inner: Bash,
}

impl BashUnsandboxed {
    #[must_use]
    pub fn new() -> Self {
        Self::with_background(None)
    }

    #[must_use]
    pub fn with_background(background: Option<Arc<keke_tasks::BackgroundTasks>>) -> Self {
        Self {
            inner: Bash {
                sandbox: Arc::new(keke_sandbox::Sandbox::unconfined()),
                background,
            },
        }
    }
}

impl Default for BashUnsandboxed {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for BashUnsandboxed {
    type Args = BashUnsandboxedArgs;
    type Output = BashOutput;

    fn id(&self) -> ToolId {
        ToolId::new("bash_unsandboxed")
    }

    fn description(&self, _ctx: &ListToolsContext) -> ToolDescription {
        ToolDescription::new(
            "Run a shell command outside the sandbox. Every call is reviewed by the guardian in Auto \
             mode, otherwise by a person, with your justification, so use it only after `bash` failed because of the sandbox — \
             the command needs the network or must write outside the workspace — and say which. \
             Never use it to avoid trying `bash` first. `background: true` explicitly requests \
             an unconfined tracked task that runs until exit or kill_task; the reviewer must \
             approve that lifetime. Approval policy Never skips execution review.",
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            approval: ApprovalRequirement::ReviewRequired,
            ..self.inner.capabilities()
        }
    }

    async fn run(&self, ctx: ToolCallContext, args: Self::Args) -> Result<Self::Output, ToolError> {
        self.inner
            .run(
                ctx,
                BashArgs {
                    command: args.command,
                    timeout_ms: args.timeout_ms,
                    background: args.background,
                    allowed_domains: Vec::new(),
                    allowed_local_ports: Vec::new(),
                    allowed_unix_sockets: Vec::new(),
                    additional_write_roots: Vec::new(),
                },
            )
            .await
    }
}
