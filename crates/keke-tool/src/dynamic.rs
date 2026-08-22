//! Object-safe erasure of [`Tool`].
//!
//! The engine holds `Arc<dyn ToolDyn>` values and speaks JSON to them. The
//! blanket impl below is the single place where argument decoding and output
//! encoding happen, so no individual tool re-implements that plumbing — and no
//! individual tool can get the error mapping subtly wrong.

use std::future::Future;
use std::pin::Pin;

use keke_protocol::ContentBlock;
use schemars::schema_for;
use serde_json::Value;

use crate::ListToolsContext;
use crate::Tool;
use crate::ToolCallContext;
use crate::ToolCapabilities;
use crate::ToolDescription;
use crate::ToolError;
use crate::ToolId;
use crate::ToolOutput;

/// A tool the engine can hold without knowing its argument type.
pub type ArcTool = std::sync::Arc<dyn ToolDyn>;

/// A tool's result after erasure.
#[derive(Clone, Debug, PartialEq)]
pub struct TypedToolOutput {
    pub tool_id: ToolId,
    /// The structured value, retained for replay and rich surfaces.
    pub value: Value,
    /// What the model sees.
    pub model_output: Vec<ContentBlock>,
}

/// The dyn-compatible face of [`Tool`].
///
/// Do not implement this directly — implement [`Tool`] and take the blanket
/// impl. Hand-implementing it would bypass the shared schema and error
/// handling, which is exactly the drift this indirection exists to prevent.
pub trait ToolDyn: Send + Sync + 'static {
    fn id(&self) -> ToolId;
    fn description(&self, ctx: &ListToolsContext) -> ToolDescription;
    fn capabilities(&self) -> ToolCapabilities;
    fn should_list(&self, ctx: &ListToolsContext) -> bool;

    /// JSON Schema for the tool's arguments, derived from its `Args` type.
    fn input_schema(&self) -> Value;

    /// Decode `args`, run the tool, and encode the outcome.
    fn call<'a>(
        &'a self,
        ctx: ToolCallContext,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = Result<TypedToolOutput, ToolError>> + Send + 'a>>;
}

impl<T: Tool> ToolDyn for T {
    fn id(&self) -> ToolId {
        Tool::id(self)
    }

    fn description(&self, ctx: &ListToolsContext) -> ToolDescription {
        Tool::description(self, ctx)
    }

    fn capabilities(&self) -> ToolCapabilities {
        Tool::capabilities(self)
    }

    fn should_list(&self, ctx: &ListToolsContext) -> bool {
        Tool::should_list(self, ctx)
    }

    fn input_schema(&self) -> Value {
        serde_json::to_value(schema_for!(T::Args))
            .unwrap_or_else(|_| Value::Object(serde_json::Map::new()))
    }

    fn call<'a>(
        &'a self,
        ctx: ToolCallContext,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = Result<TypedToolOutput, ToolError>> + Send + 'a>> {
        let tool_id = Tool::id(self);
        Box::pin(async move {
            let decoded: T::Args =
                serde_json::from_value(args).map_err(|error| ToolError::InvalidArgs {
                    tool: tool_id.to_string(),
                    message: error.to_string(),
                })?;

            let value = Tool::execute(self, ctx, decoded)
                .await
                .into_terminal()
                .await?;
            let model_output = value.render();
            let value = serde_json::to_value(&value).map_err(|error| {
                ToolError::custom("tool_output_not_serializable", error.to_string())
            })?;

            Ok(TypedToolOutput {
                tool_id,
                value,
                model_output,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use keke_paths::AbsPath;
    use keke_protocol::ToolCallId;
    use schemars::JsonSchema;
    use serde::Deserialize;
    use serde::Serialize;

    use super::*;
    use crate::ToolKind;
    use crate::ToolStream;

    #[derive(Deserialize, JsonSchema)]
    struct EchoArgs {
        text: String,
    }

    #[derive(Serialize)]
    struct EchoOut {
        echoed: String,
    }

    impl ToolOutput for EchoOut {
        fn render(&self) -> Vec<ContentBlock> {
            vec![ContentBlock::text(&self.echoed)]
        }
    }

    struct Echo;

    impl Tool for Echo {
        type Args = EchoArgs;
        type Output = EchoOut;

        fn id(&self) -> ToolId {
            ToolId::new("echo")
        }

        fn description(&self, _ctx: &ListToolsContext) -> ToolDescription {
            ToolDescription::new("Echo the input back.")
        }

        fn capabilities(&self) -> ToolCapabilities {
            ToolCapabilities::of_kind(ToolKind::Meta)
        }

        async fn run(
            &self,
            _ctx: ToolCallContext,
            args: Self::Args,
        ) -> Result<Self::Output, ToolError> {
            Ok(EchoOut { echoed: args.text })
        }
    }

    /// A tool overriding neither `run` nor `execute` must fail loudly.
    struct Silent;

    impl Tool for Silent {
        type Args = EchoArgs;
        type Output = EchoOut;

        fn id(&self) -> ToolId {
            ToolId::new("silent")
        }

        fn description(&self, _ctx: &ListToolsContext) -> ToolDescription {
            ToolDescription::new("Does nothing.")
        }
    }

    fn ctx() -> ToolCallContext {
        #[cfg(unix)]
        let root = "/tmp";
        #[cfg(windows)]
        let root = r"C:\tmp";
        ToolCallContext {
            call_id: ToolCallId::new("call-1"),
            workspace_root: AbsPath::new(root).expect("absolute"),
            cancelled: Arc::new(|| false),
        }
    }

    #[tokio::test]
    async fn blanket_impl_decodes_runs_and_encodes() {
        let tool: ArcTool = Arc::new(Echo);
        let out = tool
            .call(ctx(), serde_json::json!({ "text": "hi" }))
            .await
            .expect("call succeeds");
        assert_eq!(out.tool_id, ToolId::new("echo"));
        assert_eq!(out.value["echoed"], "hi");
        assert_eq!(out.model_output, vec![ContentBlock::text("hi")]);
    }

    #[tokio::test]
    async fn bad_arguments_report_the_tool_name() {
        let tool: ArcTool = Arc::new(Echo);
        let error = tool
            .call(ctx(), serde_json::json!({ "wrong": 1 }))
            .await
            .expect_err("decode fails");
        assert!(matches!(error, ToolError::InvalidArgs { ref tool, .. } if tool == "echo"));
    }

    #[tokio::test]
    async fn unimplemented_tool_fails_loudly() {
        let tool: ArcTool = Arc::new(Silent);
        let error = tool
            .call(ctx(), serde_json::json!({ "text": "hi" }))
            .await
            .expect_err("no implementation");
        assert!(matches!(error, ToolError::NotImplemented(_)));
    }

    #[tokio::test]
    async fn stream_without_terminal_is_a_protocol_violation() {
        let empty: ToolStream<EchoOut> =
            ToolStream::with_progress(futures::stream::empty(), Err(ToolError::Cancelled));
        // `with_progress` always appends a terminal, so the invariant holds even
        // for an empty progress stream.
        assert!(matches!(
            empty.into_terminal().await,
            Err(ToolError::Cancelled)
        ));
    }

    #[test]
    fn input_schema_comes_from_the_args_type() {
        let tool: ArcTool = Arc::new(Echo);
        let schema = tool.input_schema();
        assert!(schema["properties"]["text"].is_object());
    }
}
