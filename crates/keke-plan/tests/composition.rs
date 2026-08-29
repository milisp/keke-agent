//! Plan mode as the engine actually runs it: guard, then reviewer, then body.
//!
//! `keke_core::dispatch` is the thing under test here rather than the pieces —
//! the ordering it imposes is what makes an auto-approved plan file possible at
//! all, and what makes denial monotonic.

// A panic is the assertion here, and an integration test is not `#[cfg(test)]`,
// so the workspace's allow-expect-in-tests setting does not reach it.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use keke_config_types::ApprovalPolicy;
use keke_config_types::SessionMode;
use keke_core::ApprovalMemory;
use keke_core::Dispatch;
use keke_core::SessionModeSwitch;
use keke_core::ToolSet;
use keke_paths::AbsPath;
use keke_plugin_api::ApprovalDecision;
use keke_plugin_api::ApprovalRequest;
use keke_plugin_api::ApprovalReviewContributor;
use keke_plugin_api::ExtFuture;
use keke_plugin_api::ExtensionContext;
use keke_plugin_api::ExtensionRegistryBuilder;
use keke_plugin_api::ToolContributor;
use keke_protocol::ContentBlock;
use keke_protocol::SessionId;
use keke_protocol::ThreadId;
use keke_protocol::ToolCall;
use keke_protocol::ToolCallId;
use keke_protocol::ToolStatus;
use keke_tool::ArcTool;
use keke_tool::ListToolsContext;
use keke_tool::Tool;
use keke_tool::ToolCallContext;
use keke_tool::ToolCapabilities;
use keke_tool::ToolDescription;
use keke_tool::ToolError;
use keke_tool::ToolId;
use keke_tool::ToolKind;
use keke_tool::ToolOutput;

/// Stands in for `keke_tools::WriteFile`: this crate must not depend on the
/// built-in pack to test what it does to it.
struct WriteFile;

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct WriteArgs {
    path: String,
    content: String,
}

#[derive(serde::Serialize)]
struct WriteOut {
    path: String,
}

impl ToolOutput for WriteOut {
    fn render(&self) -> Vec<ContentBlock> {
        vec![ContentBlock::text(format!("wrote {}", self.path))]
    }
}

impl Tool for WriteFile {
    type Args = WriteArgs;
    type Output = WriteOut;

    fn id(&self) -> ToolId {
        ToolId::new("write_file")
    }

    fn description(&self, _ctx: &ListToolsContext) -> ToolDescription {
        ToolDescription::new("Write a file.")
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::of_kind(ToolKind::Edit)
    }

    async fn run(
        &self,
        _ctx: ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolError> {
        std::fs::write(&args.path, args.content.as_bytes())
            .map_err(|error| ToolError::custom("write_failed", error.to_string()))?;
        Ok(WriteOut { path: args.path })
    }
}

struct Writes;

impl ToolContributor for Writes {
    fn tools(&self, _ctx: &ExtensionContext) -> Vec<ArcTool> {
        vec![Arc::new(WriteFile)]
    }
}

/// A reviewer that says yes to everything, registered *after* plan mode's.
///
/// It is the "permissive extension" invariant 7 is about: it can rescue nothing
/// a guard already refused.
struct AlwaysAllow;

impl ApprovalReviewContributor for AlwaysAllow {
    fn review<'a>(
        &'a self,
        _ctx: &'a ExtensionContext,
        _request: &'a ApprovalRequest,
    ) -> ExtFuture<'a, Option<ApprovalDecision>> {
        Box::pin(async { Some(ApprovalDecision::Allow) })
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    plan_file: std::path::PathBuf,
    registry: keke_plugin_api::ExtensionRegistry,
    ext_ctx: ExtensionContext,
    root: AbsPath,
    memory: ApprovalMemory,
}

fn harness() -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize");
    let plan_file = root.join("plan.md");

    let mut builder = ExtensionRegistryBuilder::new();
    builder.tool_contributor(Arc::new(Writes));
    keke_plan::install(
        &mut builder,
        Arc::new(SessionModeSwitch::new(SessionMode::Plan)),
        keke_plan::PlanLocation::fixed(&plan_file),
    );
    builder.approval_review_contributor(Arc::new(AlwaysAllow));
    let registry = builder.build();

    // The tracker is reconciled and the plan file settled by the turn
    // boundary (`start_turn`), the same as in a running session.
    let ext_ctx = ExtensionContext::new(SessionId::new(), ThreadId::new());
    Harness {
        _dir: dir,
        plan_file,
        registry,
        ext_ctx,
        root: AbsPath::new(root).expect("absolute"),
        memory: ApprovalMemory::default(),
    }
}

async fn start_turn(harness: &Harness) {
    for contributor in harness.registry.turn_lifecycle_contributors() {
        contributor
            .on_turn_start(&harness.ext_ctx, keke_protocol::TurnId::new())
            .await;
    }
}

fn write_call(path: &std::path::Path) -> ToolCall {
    ToolCall {
        id: ToolCallId::new("c1"),
        name: "write_file".to_string(),
        arguments: serde_json::json!({ "path": path.display().to_string(), "content": "hello" }),
    }
}

async fn dispatch(harness: &Harness, call: &ToolCall, policy: ApprovalPolicy) -> ToolStatus {
    let tools = ToolSet::from_registry(&harness.registry, &harness.ext_ctx);
    keke_core::dispatch(
        call,
        Dispatch {
            tools: &tools,
            registry: &harness.registry,
            ext_ctx: &harness.ext_ctx,
            workspace_root: &harness.root,
            cancelled: Arc::new(|| false),
            policy,
            memory: &harness.memory,
        },
    )
    .await
    .result
    .status
}

#[tokio::test]
async fn the_guard_lets_a_plan_file_write_through_for_the_reviewer_to_allow() {
    let harness = harness();
    start_turn(&harness).await;

    let call = write_call(&harness.plan_file);
    assert_eq!(
        dispatch(&harness, &call, ApprovalPolicy::OnRequest).await,
        ToolStatus::Ok
    );
    assert_eq!(
        std::fs::read_to_string(&harness.plan_file).expect("written"),
        "hello"
    );
}

#[tokio::test]
async fn a_permissive_reviewer_cannot_rescue_a_write_the_guard_denied() {
    let harness = harness();
    start_turn(&harness).await;

    // `AlwaysAllow` is registered and would say yes; the guard runs first and
    // its denial is final, so it is never consulted.
    let call = write_call(&harness._dir.path().join("src.rs"));
    assert_eq!(
        dispatch(&harness, &call, ApprovalPolicy::OnRequest).await,
        ToolStatus::Denied
    );
}

/// Plan mode is not an approval policy, so turning approvals off does not turn
/// it off: the guard has no policy to consult.
#[tokio::test]
async fn edits_stay_refused_even_when_nothing_is_being_asked_about() {
    let harness = harness();
    start_turn(&harness).await;

    let call = write_call(&harness._dir.path().join("src.rs"));
    assert_eq!(
        dispatch(&harness, &call, ApprovalPolicy::Never).await,
        ToolStatus::Denied
    );
}
