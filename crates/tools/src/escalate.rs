//! Where the sandbox stops, a person decides.
//!
//! codex's arrangement: a command the sandbox refuses is not retried bare on
//! the model's say-so. The model asks — with a reason — and a person answers
//! each time. That is what makes the sandbox's defaults bearable: `cargo add`
//! needing the network is one approval rather than a reason to turn the
//! sandbox off.

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
    /// Why this cannot run in the sandbox, for the person deciding — what it
    /// needs (the network, a path outside the workspace, `.git`) and why.
    pub justification: String,
    /// Wall-clock budget in milliseconds, as for `bash`.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// `bash` without the sandbox, asked about every time.
///
/// Offered only where the sandbox confines something; with nothing confined
/// there is nothing to step outside of. Foreground only: a command allowed
/// out of the sandbox is one a person approved for a reason they read, and a
/// background task would go on running unconfined long after they stopped
/// watching it.
pub struct BashUnsandboxed {
    inner: Bash,
}

impl BashUnsandboxed {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Bash {
                sandbox: Arc::new(keke_sandbox::Sandbox::unconfined()),
                background: None,
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
            "Run a shell command outside the sandbox. The person is asked every time and sees \
             your justification, so use it only after `bash` failed because of the sandbox — \
             the command needs the network or must write outside the workspace — and say which. \
             Never use it to avoid trying `bash` first.",
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            approval: ApprovalRequirement::Always,
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
                    background: false,
                },
            )
            .await
    }
}
