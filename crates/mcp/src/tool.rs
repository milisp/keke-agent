use std::sync::Arc;

use keke_protocol::ContentBlock;
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
use serde_json::Value;

use crate::backend::backend;
use crate::server::McpServer;
use crate::server::RemoteTool;

/// Arguments for an MCP tool.
///
/// A free-form object, because the shape is the *server's* to declare and this
/// type is compiled once for every server there will ever be. The shape the
/// model is told to send comes from the server instead, through
/// [`Tool::input_schema_override`].
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct McpArgs(pub serde_json::Map<String, Value>);

/// What an MCP tool returned.
#[derive(Debug, Serialize)]
pub struct McpToolOutput {
    /// The namespaced id of the tool that produced this.
    pub tool: String,
    /// The server's content blocks, kept verbatim for replay.
    pub content: Vec<Value>,
}

impl ToolOutput for McpToolOutput {
    fn render(&self) -> Vec<ContentBlock> {
        let mut blocks: Vec<ContentBlock> = self
            .content
            .iter()
            .map(|block| match block.get("text").and_then(Value::as_str) {
                Some(text) => ContentBlock::text(text),
                // A block keke has no representation for is reported as what it
                // is rather than dropped, so the model is not left reasoning
                // about a result it silently never saw.
                None => ContentBlock::text(format!(
                    "[{} content omitted]",
                    block
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("unrecognized")
                )),
            })
            .collect();
        if blocks.is_empty() {
            blocks.push(ContentBlock::text("(the tool returned no content)"));
        }
        blocks
    }
}

/// One tool on one MCP server, wearing keke's tool interface.
pub(crate) struct McpTool {
    server: Arc<McpServer>,
    /// The name to send back to the server, which is not the namespaced id.
    remote_name: String,
    id: ToolId,
    description: String,
    /// The argument schema the server declared for this tool.
    input_schema: Value,
    call_timeout_millis: u64,
}

impl McpTool {
    /// Namespaced `plugin:server:tool`.
    ///
    /// Two plugins may ship servers exposing the same tool name, and
    /// `keke-plugin` deliberately does not reject that. Building the id from
    /// the owning plugin and server makes the collision impossible instead of
    /// making it something to detect and report: plugin names are unique within
    /// a resolved set, and server names are unique within a plugin.
    pub(crate) fn new(
        server: &Arc<McpServer>,
        remote: RemoteTool,
        call_timeout_millis: u64,
    ) -> Self {
        let id = ToolId::new(format!(
            "{}:{}:{}",
            server.plugin(),
            server.name(),
            remote.name
        ));
        Self {
            server: Arc::clone(server),
            remote_name: remote.name,
            id,
            description: describe(&remote.description),
            input_schema: remote.input_schema,
            call_timeout_millis,
        }
    }
}

fn describe(description: &str) -> String {
    if description.is_empty() {
        "An MCP tool. The server provided no description.".to_string()
    } else {
        description.to_string()
    }
}

impl Tool for McpTool {
    type Args = McpArgs;
    type Output = McpToolOutput;

    /// The shape comes from the server, not from [`McpArgs`].
    ///
    /// An MCP server describes its own tools, so their argument schemas are not
    /// known until keke has asked. Deriving from the open map `McpArgs` would
    /// advertise "any object", which tells the model nothing about what to
    /// send. Decoding is unaffected — arguments still arrive as `McpArgs`.
    fn input_schema_override(&self) -> Option<Value> {
        Some(self.input_schema.clone())
    }

    fn id(&self) -> ToolId {
        self.id.clone()
    }

    fn description(&self, _ctx: &ListToolsContext) -> ToolDescription {
        ToolDescription::new(self.description.clone())
    }

    /// An MCP server is an arbitrary program: keke knows nothing about what a
    /// given tool touches, so the honest classification is the unrestricted
    /// one, and with it the conservative concurrency answer.
    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            timeout_millis: Some(self.call_timeout_millis),
            ..ToolCapabilities::of_kind(ToolKind::Execute)
        }
    }

    async fn run(&self, ctx: ToolCallContext, args: Self::Args) -> Result<Self::Output, ToolError> {
        // Clamp to whatever the engine is already enforcing, so this crate does
        // not keep a second budget that can drift from the real one.
        let budget = ctx
            .timeout_millis
            .unwrap_or(self.call_timeout_millis)
            .min(self.call_timeout_millis);

        let server = Arc::clone(&self.server);
        let name = self.remote_name.clone();
        let arguments = Value::Object(args.0);

        let Some(backend) = backend() else {
            return Err(ToolError::custom(
                "mcp_unavailable",
                "keke could not start the runtime MCP servers run on",
            ));
        };

        // Every server's I/O lives on the backend runtime, including the reader
        // task opened when the tool list was fetched; the call has to join it
        // there rather than drive the connection from the session's runtime.
        let joined = backend
            .spawn(async move { server.call_tool(&name, arguments, budget).await })
            .await;

        let result = match joined {
            Ok(result) => result,
            Err(error) => Err(format!("the MCP call did not complete: {error}")),
        };

        let value = result.map_err(|message| ToolError::custom("mcp_call_failed", message))?;

        let content = value
            .get("content")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        // MCP reports a tool's own failure in-band. It is still a failure, and
        // a tool that swallowed it would hand the model a successful-looking
        // empty result.
        if value.get("isError").and_then(Value::as_bool) == Some(true) {
            let output = McpToolOutput {
                tool: self.id.to_string(),
                content,
            };
            let detail: String = output
                .render()
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            return Err(ToolError::custom("mcp_tool_error", detail));
        }

        Ok(McpToolOutput {
            tool: self.id.to_string(),
            content,
        })
    }
}
