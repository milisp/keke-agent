//! The seam, end to end: a real engine session behind a `Conversation`.
//!
//! The unit tests either side of this cover the bridge and the surface in
//! isolation. This is the one that would have caught them agreeing on a
//! contract neither actually honours.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::Mutex;

use keke_acp::PermissionAnswer;
use keke_acp::Update;
use keke_config_types::ApprovalPolicy;
use keke_config_types::CompactionConfig;
use keke_config_types::HomeLayout;
use keke_config_types::MaxOutputTokens;
use keke_config_types::ModelSelection;
use keke_core::SessionBuilder;
use keke_paths::AbsPath;
use keke_plugin_api::ExtensionContext;
use keke_plugin_api::ExtensionRegistryBuilder;
use keke_plugin_api::ToolContributor;
use keke_protocol::ContentBlock;
use keke_protocol::StopReason;
use keke_protocol::ToolCallId;
use keke_protocol::ToolStatus;
use keke_provider_api::ModelProvider;
use keke_provider_api::ModelRequest;
use keke_provider_api::ProviderError;
use keke_provider_api::ProviderFuture;
use keke_provider_api::ProviderInfo;
use keke_provider_api::StreamChunk;
use keke_provider_api::StreamEvent;
use keke_provider_api::WireApi;
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
use tokio::sync::mpsc::UnboundedReceiver;

// ----------------------------------------------------------------- a provider

struct Scripted {
    info: ProviderInfo,
    script: Mutex<Vec<Vec<StreamChunk>>>,
}

impl Scripted {
    fn new(script: Vec<Vec<StreamChunk>>) -> Self {
        Self {
            info: ProviderInfo {
                route: "scripted".to_string(),
                display_name: "Scripted".to_string(),
                base_url: "http://scripted.invalid".to_string(),
                wire_api: WireApi::ChatCompletions,
                auth_id: None,
                env_key: None,
            },
            script: Mutex::new(script),
        }
    }
}

impl ModelProvider for Scripted {
    fn info(&self) -> &ProviderInfo {
        &self.info
    }

    fn stream<'a>(
        &'a self,
        _request: ModelRequest,
    ) -> ProviderFuture<'a, Result<StreamEvent, ProviderError>> {
        Box::pin(async move {
            let turn = {
                let mut script = self.script.lock().expect("lock");
                if script.is_empty() {
                    Vec::new()
                } else {
                    script.remove(0)
                }
            };
            Ok(Box::pin(futures::stream::iter(
                turn.into_iter().map(Ok).collect::<Vec<_>>(),
            )) as StreamEvent)
        })
    }
}

// --------------------------------------------------------------------- a tool

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct Args {
    text: String,
}

#[derive(serde::Serialize)]
struct Out {
    echoed: String,
}

impl ToolOutput for Out {
    fn render(&self) -> Vec<ContentBlock> {
        vec![ContentBlock::text(&self.echoed)]
    }
}

struct Risky;

impl Tool for Risky {
    type Args = Args;
    type Output = Out;

    fn id(&self) -> ToolId {
        ToolId::new("risky")
    }

    fn description(&self, _ctx: &ListToolsContext) -> ToolDescription {
        ToolDescription::new("Runs a command.")
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::of_kind(ToolKind::Execute)
    }

    async fn run(
        &self,
        _ctx: ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolError> {
        Ok(Out { echoed: args.text })
    }
}

struct Pack;

impl ToolContributor for Pack {
    fn tools(&self, _ctx: &ExtensionContext) -> Vec<ArcTool> {
        vec![Arc::new(Risky)]
    }
}

// -------------------------------------------------------------------- harness

fn tool_turn() -> Vec<StreamChunk> {
    let id = ToolCallId::new("call-1");
    vec![
        StreamChunk::ToolCallStart {
            id: id.clone(),
            name: "risky".to_string(),
        },
        StreamChunk::ToolCallArgsDelta {
            id: id.clone(),
            delta: "{\"text\":\"ok\"}".to_string(),
        },
        StreamChunk::ToolCallEnd { id },
        StreamChunk::Done(StopReason::ToolUse),
    ]
}

fn text_turn(text: &str) -> Vec<StreamChunk> {
    vec![
        StreamChunk::TextDelta(text.to_string()),
        StreamChunk::Done(StopReason::EndTurn),
    ]
}

struct Started {
    conversation: Arc<dyn keke_acp::Conversation>,
    updates: UnboundedReceiver<Update>,
    _dir: tempfile::TempDir,
}

async fn start(script: Vec<Vec<StreamChunk>>, approval: ApprovalPolicy) -> Started {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = std::fs::canonicalize(dir.path()).expect("canonicalize");
    let root = AbsPath::new(root).expect("absolute");

    let (approvals, requests) = keke_acp::approvals();
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.tool_contributor(Arc::new(Pack));
    keke_acp::install(&mut extensions, Arc::clone(&approvals));

    let builder = SessionBuilder::new()
        .config(keke_core::SessionConfig {
            model: ModelSelection {
                provider: "scripted".to_string(),
                model: "test".to_string(),
            },
            home: HomeLayout {
                home: root.clone(),
                workspace_root: root,
            },
            max_output_tokens: MaxOutputTokens::default(),
            reasoning_effort: None,
            compaction: CompactionConfig::default(),
            checkpoints: keke_config_types::CheckpointConfig::default(),
            approval,
        })
        .provider(Arc::new(Scripted::new(script)))
        .extensions(extensions.build());

    let opened = keke_acp::local(builder, approvals, requests)
        .await
        .expect("a session");
    let (conversation, updates) = (opened.conversation, opened.updates);
    Started {
        conversation,
        updates,
        _dir: dir,
    }
}

/// Collect updates until the turn ends, so a test never waits on a stream that
/// has already said everything it is going to say.
async fn until_turn_end(updates: &mut UnboundedReceiver<Update>) -> Vec<Update> {
    let mut seen = Vec::new();
    while let Some(update) = updates.recv().await {
        let last = matches!(update, Update::TurnEnded(_) | Update::Failed(_));
        seen.push(update);
        if last {
            break;
        }
    }
    seen
}

// ---------------------------------------------------------------------- tests

#[tokio::test]
async fn a_prompt_streams_text_and_ends_the_turn() {
    let mut started = start(vec![text_turn("hello")], ApprovalPolicy::Never).await;
    started
        .conversation
        .prompt("hi".to_string())
        .await
        .expect("the prompt is accepted");

    let seen = until_turn_end(&mut started.updates).await;
    assert!(seen.contains(&Update::TurnStarted));
    assert!(seen.contains(&Update::TextDelta("hello".to_string())));
    assert!(matches!(seen.last(), Some(Update::TurnEnded(_))));
}

/// The whole point of the surface having a permission answer: something has to
/// have asked.
#[tokio::test]
async fn a_call_needing_approval_reaches_the_surface_and_the_answer_reaches_the_engine() {
    let mut started = start(
        vec![tool_turn(), text_turn("done")],
        ApprovalPolicy::OnRequest,
    )
    .await;

    let conversation = Arc::clone(&started.conversation);
    tokio::spawn(async move {
        let _ = conversation.prompt("go".to_string()).await;
    });

    let mut seen = Vec::new();
    while let Some(update) = started.updates.recv().await {
        if let Update::PermissionRequested { id, call, reason } = &update {
            assert_eq!(call.name, "risky");
            assert!(!reason.is_empty(), "a prompt must say why");
            started
                .conversation
                .respond_to_permission(id, PermissionAnswer::Allow, None);
        }
        let last = matches!(update, Update::TurnEnded(_) | Update::Failed(_));
        seen.push(update);
        if last {
            break;
        }
    }

    assert!(
        seen.iter()
            .any(|update| matches!(update, Update::PermissionRequested { .. })),
        "the surface was never asked: {seen:?}"
    );
    let ended = seen
        .iter()
        .find_map(|update| match update {
            Update::ToolCallEnded(result) => Some(result),
            _ => None,
        })
        .expect("the tool ran");
    assert_eq!(
        ended.status,
        ToolStatus::Ok,
        "the answer did not get through"
    );
}

/// Denying is not a failure; the model is told and the turn goes on.
#[tokio::test]
async fn denying_refuses_the_call_without_ending_the_conversation() {
    let mut started = start(
        vec![tool_turn(), text_turn("understood")],
        ApprovalPolicy::OnRequest,
    )
    .await;

    let conversation = Arc::clone(&started.conversation);
    tokio::spawn(async move {
        let _ = conversation.prompt("go".to_string()).await;
    });

    let mut status = None;
    while let Some(update) = started.updates.recv().await {
        match &update {
            Update::PermissionRequested { id, .. } => {
                started
                    .conversation
                    .respond_to_permission(id, PermissionAnswer::Deny, None);
            }
            Update::ToolCallEnded(result) => status = Some(result.status),
            Update::TurnEnded(_) | Update::Failed(_) => break,
            _ => {}
        }
    }
    assert_eq!(status, Some(ToolStatus::Denied));
}

/// A turn parked on a prompt nobody will answer is a hang. Ctrl-C has to reach
/// it, or the interface is stuck with no way out but killing the process.
#[tokio::test]
async fn cancelling_releases_a_turn_blocked_on_a_prompt() {
    let mut started = start(
        vec![tool_turn(), text_turn("never")],
        ApprovalPolicy::OnRequest,
    )
    .await;

    let conversation = Arc::clone(&started.conversation);
    tokio::spawn(async move {
        let _ = conversation.prompt("go".to_string()).await;
    });

    while let Some(update) = started.updates.recv().await {
        if matches!(update, Update::PermissionRequested { .. }) {
            break;
        }
    }
    started.conversation.cancel();

    let rest = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        until_turn_end(&mut started.updates),
    )
    .await
    .expect("the turn must not stay parked");
    assert!(
        rest.iter()
            .any(|update| matches!(update, Update::TurnEnded(StopReason::Cancelled))),
        "{rest:?}"
    );
}
