//! Tool dispatch.
//!
//! The order of checks is the policy, and it is deliberate:
//!
//! 1. The tool must exist.
//! 2. Lifecycle contributors observe the call (they cannot block it).
//! 3. Guards run and may deny — and only deny.
//! 4. The tool body runs, under the budget it advertised.
//!
//! Putting guards after the observers, and giving them no way to allow, is what
//! makes denial monotonic: no registration order can turn a denial back into
//! permission.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use keke_paths::AbsPath;
use keke_plugin_api::ExtensionContext;
use keke_plugin_api::ExtensionRegistry;
use keke_protocol::ContentBlock;
use keke_protocol::ToolCall;
use keke_protocol::ToolResult;
use keke_protocol::ToolStatus;
use keke_tool::ArcTool;
use keke_tool::ToolCallContext;
use keke_tool::ToolError;

/// The tools available for a session, keyed by the name the model sees.
#[derive(Clone, Default)]
pub struct ToolSet {
    tools: BTreeMap<String, ArcTool>,
}

impl ToolSet {
    /// Collect every contributed tool.
    ///
    /// A later contributor shadowing an earlier one's id is allowed and
    /// intentional: it is how a plugin replaces a built-in implementation.
    pub fn from_registry(registry: &ExtensionRegistry, ctx: &ExtensionContext) -> Self {
        let mut tools = BTreeMap::new();
        for contributor in registry.tool_contributors() {
            for tool in contributor.tools(ctx) {
                tools.insert(tool.id().to_string(), tool);
            }
        }
        Self { tools }
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&ArcTool> {
        self.tools.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &ArcTool> {
        self.tools.values()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

/// Run one tool call through the full policy pipeline.
pub async fn dispatch(
    call: &ToolCall,
    tools: &ToolSet,
    registry: &ExtensionRegistry,
    ext_ctx: &ExtensionContext,
    workspace_root: &AbsPath,
    cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
) -> ToolResult {
    let Some(tool) = tools.get(&call.name) else {
        // An unknown tool is the model's mistake and the model can correct it,
        // so this is an error result rather than an aborted turn.
        return ToolResult::error(
            call.id.clone(),
            format!(
                "unknown tool `{}`; available tools: {}",
                call.name,
                tools
                    .iter()
                    .map(|tool| tool.id().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
    };

    for contributor in registry.tool_lifecycle_contributors() {
        contributor.on_tool_start(ext_ctx, call).await;
    }

    if let Some(reason) = registry.first_denial(call) {
        let result = ToolResult {
            id: call.id.clone(),
            status: ToolStatus::Denied,
            content: vec![ContentBlock::text(format!("Denied: {reason}"))],
            value: None,
        };
        let error = ToolError::denied(reason);
        for contributor in registry.tool_lifecycle_contributors() {
            contributor.on_tool_finish(ext_ctx, call, Err(&error)).await;
        }
        return result;
    }

    // The advertised budget is enforced here rather than trusted to the tool.
    // A tool keeping its own copy of the number would let the two drift, and a
    // tool that simply ignored it would overrun with nothing to stop it.
    let budget = tool.capabilities().timeout_millis;
    let ctx = ToolCallContext {
        call_id: call.id.clone(),
        workspace_root: workspace_root.clone(),
        timeout_millis: budget,
        cancelled,
    };

    let running = tool.call(ctx, call.arguments.clone());
    let outcome = match budget {
        Some(millis) => {
            match tokio::time::timeout(Duration::from_millis(millis), running).await {
                Ok(outcome) => outcome,
                // Dropping the future drops whatever the tool was holding; tools
                // that spawn children set `kill_on_drop`, so this really stops.
                Err(_) => Err(ToolError::Timeout { millis }),
            }
        }
        None => running.await,
    };
    let result = match &outcome {
        Ok(output) => ToolResult {
            id: call.id.clone(),
            status: ToolStatus::Ok,
            content: output.model_output.clone(),
            value: Some(output.value.clone()),
        },
        Err(error) => ToolResult {
            id: call.id.clone(),
            status: error.status(),
            content: vec![ContentBlock::text(error.to_string())],
            value: None,
        },
    };

    let observed = outcome.as_ref().map(|_| ());
    for contributor in registry.tool_lifecycle_contributors() {
        contributor.on_tool_finish(ext_ctx, call, observed).await;
    }

    result
}
