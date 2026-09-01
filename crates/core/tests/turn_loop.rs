//! End-to-end turn-loop tests against a scripted provider.
//!
//! These exist to prove the contract tier actually composes: a provider, a tool,
//! and an extension registry meeting in a real turn. Every bug the contracts had
//! showed up here first.

// An integration test is not `#[cfg(test)]`, so the workspace's
// allow-expect-in-tests setting does not reach it. A panic is the assertion.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::Mutex;

use futures::StreamExt;
use keke_config_types::ApprovalPolicy;
use keke_config_types::CheckpointConfig;
use keke_config_types::CompactionConfig;
use keke_config_types::HomeLayout;
use keke_config_types::MaxOutputTokens;
use keke_config_types::ModelSelection;
use keke_core::SessionBuilder;
use keke_core::TurnUpdate;
use keke_core::read_log;
use keke_paths::AbsPath;
use keke_plugin_api::ApprovalDecision;
use keke_plugin_api::ApprovalRequest;
use keke_plugin_api::ApprovalReviewContributor;
use keke_plugin_api::ExtensionContext;
use keke_plugin_api::ExtensionRegistryBuilder;
use keke_plugin_api::ToolContributor;
use keke_protocol::ContentBlock;
use keke_protocol::Message;
use keke_protocol::RewindScope;
use keke_protocol::SessionEvent;
use keke_protocol::StopReason;
use keke_protocol::ToolCallId;
use keke_protocol::ToolStatus;
use keke_protocol::Usage;
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

// ---------------------------------------------------------------- scripted provider

/// A provider that replays a prepared script, one entry per model call.
struct ScriptedProvider {
    info: ProviderInfo,
    script: Mutex<Vec<Vec<StreamChunk>>>,
    /// Every request the engine made, so a test can assert what the model saw.
    seen: Arc<Mutex<Vec<ModelRequest>>>,
}

impl ScriptedProvider {
    fn new(script: Vec<Vec<StreamChunk>>) -> (Arc<Self>, Arc<Mutex<Vec<ModelRequest>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(Self {
            info: ProviderInfo {
                route: "scripted".to_string(),
                display_name: "Scripted".to_string(),
                base_url: "http://localhost".to_string(),
                wire_api: WireApi::ChatCompletions,
                auth_id: None,
                env_key: None,
            },
            script: Mutex::new(script),
            seen: Arc::clone(&seen),
        });
        (provider, seen)
    }
}

impl ModelProvider for ScriptedProvider {
    fn info(&self) -> &ProviderInfo {
        &self.info
    }

    fn stream<'a>(
        &'a self,
        request: ModelRequest,
    ) -> ProviderFuture<'a, Result<StreamEvent, ProviderError>> {
        Box::pin(async move {
            self.seen.lock().expect("lock").push(request);
            let mut script = self.script.lock().expect("lock");
            if script.is_empty() {
                return Err(ProviderError::Protocol("script exhausted".to_string()));
            }
            let chunks = script.remove(0);
            Ok(futures::stream::iter(chunks.into_iter().map(Ok)).boxed())
        })
    }
}

// ---------------------------------------------------------------- a test tool

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct EchoArgs {
    text: String,
}

#[derive(serde::Serialize)]
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

/// Advertises a tiny budget and then ignores it. The engine must stop it
/// anyway — a tool cannot be trusted to enforce its own timeout.
struct Overrunning;

impl Tool for Overrunning {
    type Args = EchoArgs;
    type Output = EchoOut;

    fn id(&self) -> ToolId {
        ToolId::new("overrunning")
    }

    fn description(&self, _ctx: &ListToolsContext) -> ToolDescription {
        ToolDescription::new("Sleeps well past its advertised budget.")
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            timeout_millis: Some(80),
            ..ToolCapabilities::of_kind(ToolKind::Meta)
        }
    }

    async fn run(
        &self,
        _ctx: ToolCallContext,
        _args: Self::Args,
    ) -> Result<Self::Output, ToolError> {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        Ok(EchoOut {
            echoed: "never".to_string(),
        })
    }
}

/// Claims to run commands, so the policy has something to object to.
struct Dangerous;

impl Tool for Dangerous {
    type Args = EchoArgs;
    type Output = EchoOut;

    fn id(&self) -> ToolId {
        ToolId::new("dangerous")
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
        Ok(EchoOut { echoed: args.text })
    }
}

struct EchoPack;

impl ToolContributor for EchoPack {
    fn tools(&self, _ctx: &ExtensionContext) -> Vec<ArcTool> {
        vec![
            Arc::new(Echo),
            Arc::new(Overrunning),
            Arc::new(Dangerous),
            Arc::new(WriteFile),
        ]
    }
}

/// Writes a file, so a turn has something to be undone.
#[derive(serde::Deserialize, schemars::JsonSchema)]
struct WriteArgs {
    path: String,
    text: String,
}

struct WriteFile;

impl Tool for WriteFile {
    type Args = WriteArgs;
    type Output = EchoOut;

    fn id(&self) -> ToolId {
        ToolId::new("write_file")
    }

    fn description(&self, _ctx: &ListToolsContext) -> ToolDescription {
        ToolDescription::new("Write a file.")
    }

    /// `Edit`, which is what tells the engine to snapshot the tree before this
    /// runs.
    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::of_kind(ToolKind::Edit)
    }

    async fn run(&self, ctx: ToolCallContext, args: Self::Args) -> Result<Self::Output, ToolError> {
        let path = ctx.workspace_root.as_path().join(&args.path);
        std::fs::write(&path, &args.text)
            .map_err(|error| ToolError::custom("write_failed", error.to_string()))?;
        Ok(EchoOut { echoed: args.path })
    }
}

/// A reviewer that answers every request the same way, and counts the asking.
struct Reviewer {
    decision: ApprovalDecision,
    asked: Arc<Mutex<Vec<String>>>,
}

impl Reviewer {
    fn new(decision: ApprovalDecision) -> (Arc<Self>, Arc<Mutex<Vec<String>>>) {
        let asked = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(Self {
                decision,
                asked: Arc::clone(&asked),
            }),
            asked,
        )
    }
}

impl ApprovalReviewContributor for Reviewer {
    fn review<'a>(
        &'a self,
        _ctx: &'a ExtensionContext,
        request: &'a ApprovalRequest,
    ) -> keke_plugin_api::ExtFuture<'a, Option<ApprovalDecision>> {
        Box::pin(async move {
            self.asked
                .lock()
                .expect("lock")
                .push(request.call.name.clone());
            Some(self.decision.clone())
        })
    }
}

// ---------------------------------------------------------------- harness

struct Harness {
    _dir: tempfile::TempDir,
    home: HomeLayout,
}

fn harness() -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = std::fs::canonicalize(dir.path()).expect("canonicalize");
    let root = AbsPath::new(root).expect("absolute");
    Harness {
        _dir: dir,
        home: HomeLayout {
            home: root.clone(),
            workspace_root: root,
        },
    }
}

fn session_config(home: &HomeLayout) -> keke_core::SessionConfig {
    // These tests are about the loop, not about policy; the approval tests below
    // opt in explicitly so nothing else has to think about it.
    session_config_with(home, ApprovalPolicy::Never)
}

fn session_config_with(home: &HomeLayout, approval: ApprovalPolicy) -> keke_core::SessionConfig {
    keke_core::SessionConfig {
        model: ModelSelection {
            provider: "scripted".to_string(),
            model: "test-model".to_string(),
        },
        home: home.clone(),
        max_output_tokens: MaxOutputTokens::default(),
        reasoning_effort: None,
        service_tier: None,
        compaction: CompactionConfig::default(),
        checkpoints: CheckpointConfig::default(),
        approval,
    }
}

fn text_reply(text: &str) -> Vec<StreamChunk> {
    vec![
        StreamChunk::TextDelta(text.to_string()),
        StreamChunk::Usage(Usage {
            input_tokens: 10,
            output_tokens: 5,
            ..Usage::default()
        }),
        StreamChunk::Done(StopReason::EndTurn),
    ]
}

// ---------------------------------------------------------------- tests

#[tokio::test]
async fn a_plain_turn_streams_text_and_logs_everything() {
    let harness = harness();
    let (provider, seen) = ScriptedProvider::new(vec![text_reply("hello there")]);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    let mut session = SessionBuilder::new()
        .config(session_config(&harness.home))
        .provider(provider)
        .updates(tx)
        .build()
        .await
        .expect("builds");

    let outcome = session
        .run_turn(Message::user("hi"))
        .await
        .expect("turn completes");

    assert_eq!(outcome.stop_reason, StopReason::EndTurn);
    assert_eq!(outcome.usage.input_tokens, 10);
    assert_eq!(
        outcome.message.as_ref().map(Message::text).as_deref(),
        Some("hello there")
    );

    // The model saw exactly one request, carrying a system prompt.
    let requests = seen.lock().expect("lock");
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0]
            .system
            .as_ref()
            .expect("system")
            .contains("keke")
    );

    // Everything model-visible is on disk.
    let log = read_log(session.log_path()).expect("reads");
    let kinds: Vec<&str> = log
        .iter()
        .map(|entry| match &entry.event {
            SessionEvent::SessionStart { .. } => "session_start",
            SessionEvent::TurnStart { .. } => "turn_start",
            SessionEvent::ModelRequest { .. } => "model_request",
            SessionEvent::ModelResponse { .. } => "model_response",
            SessionEvent::TurnEnd { .. } => "turn_end",
            SessionEvent::ContextFragment { .. } => "context_fragment",
            _ => "other",
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            "session_start",
            "turn_start",
            // The system prompt is model-visible input that `model_request`
            // does not carry, so each fragment of it is logged in assembled
            // order, ahead of the request it is part of.
            "context_fragment",
            "context_fragment",
            "model_request",
            "model_response",
            "turn_end"
        ]
    );

    rx.close();
    let mut deltas = Vec::new();
    while let Ok(update) = rx.try_recv() {
        if let TurnUpdate::TextDelta { delta, .. } = update {
            deltas.push(delta);
        }
    }
    assert_eq!(deltas, vec!["hello there".to_string()]);
}

#[tokio::test]
async fn a_tool_call_runs_and_its_result_reaches_the_next_request() {
    let harness = harness();
    let call_id = ToolCallId::new("call-1");
    let (provider, seen) = ScriptedProvider::new(vec![
        vec![
            StreamChunk::ToolCallStart {
                id: call_id.clone(),
                name: "echo".to_string(),
            },
            StreamChunk::ToolCallArgsDelta {
                id: call_id.clone(),
                delta: "{\"text\":\"pi".to_string(),
            },
            StreamChunk::ToolCallArgsDelta {
                id: call_id.clone(),
                delta: "ng\"}".to_string(),
            },
            StreamChunk::ToolCallEnd { id: call_id },
            StreamChunk::Done(StopReason::ToolUse),
        ],
        text_reply("done"),
    ]);

    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.tool_contributor(Arc::new(EchoPack));

    let mut session = SessionBuilder::new()
        .config(session_config(&harness.home))
        .provider(provider)
        .extensions(extensions.build())
        .build()
        .await
        .expect("builds");

    let outcome = session
        .run_turn(Message::user("echo ping"))
        .await
        .expect("turn completes");
    assert_eq!(outcome.stop_reason, StopReason::EndTurn);

    // Two model calls: the tool request, then the follow-up carrying its result.
    let requests = seen.lock().expect("lock");
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0].tools.iter().any(|spec| spec.name == "echo"),
        "the tool must be advertised"
    );

    let results: Vec<_> = requests[1]
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].status, ToolStatus::Ok);
    assert_eq!(results[0].content, vec![ContentBlock::text("ping")]);
}

#[tokio::test]
async fn a_guard_denies_a_tool_and_the_model_is_told_so() {
    let harness = harness();
    let call_id = ToolCallId::new("call-1");
    let (provider, seen) = ScriptedProvider::new(vec![
        vec![
            StreamChunk::ToolCallStart {
                id: call_id.clone(),
                name: "echo".to_string(),
            },
            StreamChunk::ToolCallArgsDelta {
                id: call_id.clone(),
                delta: "{\"text\":\"x\"}".to_string(),
            },
            StreamChunk::ToolCallEnd { id: call_id },
            StreamChunk::Done(StopReason::ToolUse),
        ],
        text_reply("understood"),
    ]);

    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.tool_contributor(Arc::new(EchoPack));
    extensions.tool_guard(Box::new(|call| {
        (call.name == "echo").then(|| "echo is disabled here".to_string())
    }));

    let mut session = SessionBuilder::new()
        .config(session_config(&harness.home))
        .provider(provider)
        .extensions(extensions.build())
        .build()
        .await
        .expect("builds");

    session
        .run_turn(Message::user("echo x"))
        .await
        .expect("turn completes");

    let requests = seen.lock().expect("lock");
    let denied = requests[1]
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .find_map(|block| match block {
            ContentBlock::ToolResult(result) => Some(result),
            _ => None,
        })
        .expect("a tool result");

    assert_eq!(denied.status, ToolStatus::Denied);
    assert!(
        denied.content.iter().any(|block| matches!(
            block,
            ContentBlock::Text { text } if text.contains("echo is disabled here")
        )),
        "the denial reason must reach the model: {:?}",
        denied.content
    );
}

#[tokio::test]
async fn an_unknown_tool_is_reported_to_the_model_rather_than_aborting() {
    let harness = harness();
    let call_id = ToolCallId::new("call-1");
    let (provider, seen) = ScriptedProvider::new(vec![
        vec![
            StreamChunk::ToolCallStart {
                id: call_id.clone(),
                name: "no_such_tool".to_string(),
            },
            StreamChunk::ToolCallEnd { id: call_id },
            StreamChunk::Done(StopReason::ToolUse),
        ],
        text_reply("sorry"),
    ]);

    let mut session = SessionBuilder::new()
        .config(session_config(&harness.home))
        .provider(provider)
        .build()
        .await
        .expect("builds");

    let outcome = session
        .run_turn(Message::user("do something"))
        .await
        .expect("the turn survives");
    assert_eq!(outcome.stop_reason, StopReason::EndTurn);

    let requests = seen.lock().expect("lock");
    assert_eq!(
        requests.len(),
        2,
        "the model gets a chance to correct itself"
    );
}

#[tokio::test]
async fn a_stream_without_a_terminal_chunk_is_a_protocol_error() {
    let harness = harness();
    let (provider, _seen) =
        ScriptedProvider::new(vec![vec![StreamChunk::TextDelta("truncated".to_string())]]);

    let mut session = SessionBuilder::new()
        .config(session_config(&harness.home))
        .provider(provider)
        .build()
        .await
        .expect("builds");

    let error = session
        .run_turn(Message::user("hi"))
        .await
        .expect_err("no terminal chunk");
    assert!(
        matches!(
            error,
            keke_core::CoreError::Provider(ProviderError::Protocol(_))
        ),
        "{error}"
    );

    // A failed turn is still recorded, so the session stays replayable.
    let log = read_log(session.log_path()).expect("reads");
    assert!(
        log.iter()
            .any(|entry| matches!(entry.event, SessionEvent::Error { .. })),
        "the failure must be logged"
    );
}

#[tokio::test]
async fn the_request_that_is_logged_is_the_request_that_is_sent() {
    let harness = harness();
    let (provider, seen) = ScriptedProvider::new(vec![text_reply("ok")]);

    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.tool_contributor(Arc::new(EchoPack));

    let mut session = SessionBuilder::new()
        .config(session_config(&harness.home))
        .provider(provider)
        .extensions(extensions.build())
        .build()
        .await
        .expect("builds");

    session
        .run_turn(Message::user("hi"))
        .await
        .expect("completes");

    let log = read_log(session.log_path()).expect("reads");
    let logged = log
        .iter()
        .find_map(|entry| match &entry.event {
            SessionEvent::ModelRequest {
                messages, tools, ..
            } => Some((messages, tools)),
            _ => None,
        })
        .expect("a logged request");
    let sent = &seen.lock().expect("lock")[0];

    assert_eq!(logged.0, &sent.messages);
    assert_eq!(
        logged.1,
        &sent
            .tools
            .iter()
            .map(|spec| spec.name.clone())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn the_engine_enforces_a_budget_the_tool_ignores() {
    let harness = harness();
    let call_id = ToolCallId::new("call-1");
    let (provider, seen) = ScriptedProvider::new(vec![
        vec![
            StreamChunk::ToolCallStart {
                id: call_id.clone(),
                name: "overrunning".to_string(),
            },
            StreamChunk::ToolCallArgsDelta {
                id: call_id.clone(),
                delta: "{\"text\":\"x\"}".to_string(),
            },
            StreamChunk::ToolCallEnd { id: call_id },
            StreamChunk::Done(StopReason::ToolUse),
        ],
        text_reply("noted"),
    ]);

    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.tool_contributor(Arc::new(EchoPack));

    let mut session = SessionBuilder::new()
        .config(session_config(&harness.home))
        .provider(provider)
        .extensions(extensions.build())
        .build()
        .await
        .expect("builds");

    let started = std::time::Instant::now();
    session
        .run_turn(Message::user("run it"))
        .await
        .expect("the turn survives");

    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "the engine must not wait for a tool that overruns: took {:?}",
        started.elapsed()
    );

    let requests = seen.lock().expect("lock");
    let result = requests[1]
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .find_map(|block| match block {
            ContentBlock::ToolResult(result) => Some(result),
            _ => None,
        })
        .expect("a tool result");

    // A timeout is the harness cancelling, not the tool failing.
    assert_eq!(result.status, ToolStatus::Cancelled);
}

/// A TUI runs many turns in one session, so the history has to carry and the
/// cancel flag must not.
#[tokio::test]
async fn a_second_turn_sees_the_first_and_starts_uncancelled() {
    let harness = harness();
    let call_id = ToolCallId::new("call-1");
    let (provider, seen) = ScriptedProvider::new(vec![
        text_reply("first"),
        vec![
            StreamChunk::ToolCallStart {
                id: call_id.clone(),
                name: "echo".to_string(),
            },
            StreamChunk::ToolCallArgsDelta {
                id: call_id.clone(),
                delta: "{\"text\":\"x\"}".to_string(),
            },
            StreamChunk::ToolCallEnd { id: call_id },
            StreamChunk::Done(StopReason::ToolUse),
        ],
        text_reply("second"),
        text_reply("third"),
    ]);

    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.tool_contributor(Arc::new(EchoPack));

    let mut session = SessionBuilder::new()
        .config(session_config(&harness.home))
        .provider(provider)
        .extensions(extensions.build())
        .build()
        .await
        .expect("builds");

    session
        .run_turn(Message::user("one"))
        .await
        .expect("turn one");

    // A cancelled turn must not poison the next one: pressing Ctrl-C and then
    // asking another question is the commonest thing a person does. The flag is
    // only read after a tool batch, so the next turn has to use a tool for a
    // stale one to show itself.
    session.cancel();
    let second = session
        .run_turn(Message::user("two"))
        .await
        .expect("turn two");
    assert_eq!(
        second.stop_reason,
        StopReason::EndTurn,
        "a cancel from a finished turn must not end the next one"
    );

    let outcome = session
        .run_turn(Message::user("three"))
        .await
        .expect("turn three");
    assert_eq!(
        outcome.stop_reason,
        StopReason::EndTurn,
        "a stale cancel flag must not end a later turn"
    );

    // The third request carries everything before it.
    let requests = seen.lock().expect("lock");
    let transcript: Vec<String> = requests
        .last()
        .expect("a request")
        .messages
        .iter()
        .map(Message::text)
        .collect();
    assert!(
        transcript.contains(&"one".to_string()) && transcript.contains(&"first".to_string()),
        "the first turn must still be in view: {transcript:?}"
    );
}

/// A session that never compacts works until the provider rejects the request
/// mid-conversation, with no way forward but starting over.
#[tokio::test]
async fn a_history_past_its_budget_is_summarized_before_the_next_turn() {
    let harness = harness();
    let (provider, seen) = ScriptedProvider::new(vec![
        text_reply("first"),
        // The summarization call, then the turn it made room for.
        text_reply("NOTES: user asked about parsing; parser.rs was fixed."),
        text_reply("second"),
    ]);

    let mut config = session_config(&harness.home);
    config.compaction = CompactionConfig {
        trigger_percent: 50,
        keep_recent_messages: 1,
        context_window: 100,
    };

    let mut session = SessionBuilder::new()
        .config(config)
        .provider(provider)
        .build()
        .await
        .expect("builds");

    // Long enough to blow a 100-token window at 50%.
    session
        .run_turn(Message::user("q".repeat(1200)))
        .await
        .expect("turn one");
    session
        .run_turn(Message::user("and now?"))
        .await
        .expect("turn two");

    let requests = seen.lock().expect("lock");
    assert_eq!(requests.len(), 3, "one summarization plus two turns");

    // The summarization call carries the instruction and offers no tools: it is
    // keke asking the model for notes, not a turn.
    let summarizing = &requests[1];
    assert!(summarizing.tools.is_empty());
    assert!(
        summarizing
            .messages
            .last()
            .expect("an instruction")
            .text()
            .contains("Summarize the conversation"),
        "{:?}",
        summarizing.messages.last()
    );

    // The turn that follows sees the summary, not the original bulk.
    let transcript: Vec<String> = requests[2].messages.iter().map(Message::text).collect();
    assert!(
        transcript
            .iter()
            .any(|text| text.contains("parser.rs was fixed")),
        "the summary must be in view: {transcript:?}"
    );
    assert!(
        !transcript.iter().any(|text| text.len() > 1000),
        "the bulk it replaced must not be: {:?}",
        transcript.iter().map(String::len).collect::<Vec<_>>()
    );

    // What the model saw is reconstructable from the log, including what
    // stopped being visible.
    let log = read_log(session.log_path()).expect("reads");
    let compacted = log
        .iter()
        .find_map(|entry| match &entry.event {
            SessionEvent::Compacted {
                removed_messages, ..
            } => Some(*removed_messages),
            _ => None,
        })
        .expect("a compaction event");
    assert!(compacted > 0);
}

// ------------------------------------------------------------------- approval

/// Two calls to the same tool, so a standing permission has something to cover.
fn dangerous_calls() -> Vec<Vec<StreamChunk>> {
    let mut turns = Vec::new();
    for index in 0..2 {
        let id = ToolCallId::new(format!("call-{index}"));
        turns.push(vec![
            StreamChunk::ToolCallStart {
                id: id.clone(),
                name: "dangerous".to_string(),
            },
            StreamChunk::ToolCallArgsDelta {
                id: id.clone(),
                delta: "{\"text\":\"rm -rf /\"}".to_string(),
            },
            StreamChunk::ToolCallEnd { id },
            StreamChunk::Done(StopReason::ToolUse),
        ]);
    }
    turns.push(text_reply("done"));
    turns
}

async fn run_with_reviewer(
    decision: Option<ApprovalDecision>,
    policy: ApprovalPolicy,
) -> (Vec<SessionEvent>, Vec<String>) {
    let harness = harness();
    let (provider, _seen) = ScriptedProvider::new(dangerous_calls());

    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.tool_contributor(Arc::new(EchoPack));
    let asked = match decision {
        Some(decision) => {
            let (reviewer, asked) = Reviewer::new(decision);
            extensions.approval_review_contributor(reviewer);
            asked
        }
        None => Arc::new(Mutex::new(Vec::new())),
    };

    let mut session = SessionBuilder::new()
        .config(session_config_with(&harness.home, policy))
        .provider(provider)
        .extensions(extensions.build())
        .build()
        .await
        .expect("builds");

    let log = session.log_path().to_path_buf();
    session.run_turn(Message::user("go")).await.expect("turn");
    drop(session);

    let events = read_log(&log).expect("log");
    let events = events.into_iter().map(|entry| entry.event).collect();
    let asked = asked.lock().expect("lock").clone();
    (events, asked)
}

fn statuses(events: &[SessionEvent]) -> Vec<ToolStatus> {
    events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::ToolCallEnd { result, .. } => Some(result.status),
            _ => None,
        })
        .collect()
}

/// The reviewers were registered long before anything consulted them. This is
/// the test that says something does.
#[tokio::test]
async fn a_call_needing_approval_is_put_to_the_reviewer() {
    let (events, asked) = run_with_reviewer(
        Some(ApprovalDecision::Allow { note: None }),
        ApprovalPolicy::OnRequest,
    )
    .await;
    assert_eq!(
        asked,
        vec!["dangerous".to_string(), "dangerous".to_string()]
    );
    assert_eq!(statuses(&events), vec![ToolStatus::Ok, ToolStatus::Ok]);
}

/// Someone who approves while asking for one thing to be different has
/// instructed the work the call is about to do. The words have to reach the
/// model with the answer — said afterwards they would arrive once the thing
/// they were meant to shape had already been done.
#[tokio::test]
async fn what_a_person_says_while_approving_reaches_the_model_with_the_result() {
    let (events, _asked) = run_with_reviewer(
        Some(ApprovalDecision::Allow {
            note: Some("keep it under fifty lines".to_string()),
        }),
        ApprovalPolicy::OnRequest,
    )
    .await;

    let said: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::ToolCallEnd { result, .. } => Some(result),
            _ => None,
        })
        .flat_map(|result| result.content.iter())
        .filter_map(|block| match block {
            keke_protocol::ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        said.iter()
            .any(|text| text.contains("keep it under fifty lines")),
        "the note is nowhere in what the model was told: {said:?}"
    );
}

/// A prompt nobody is listening to is a denial, not a permission. A harness
/// that ran unreviewed commands because its surface was non-interactive would
/// be worse than one that refused.
#[tokio::test]
async fn a_call_needing_approval_with_nobody_to_ask_is_refused() {
    let (events, _asked) = run_with_reviewer(None, ApprovalPolicy::OnRequest).await;
    assert_eq!(
        statuses(&events),
        vec![ToolStatus::Denied, ToolStatus::Denied]
    );
}

#[tokio::test]
async fn a_policy_of_never_does_not_ask() {
    let (events, asked) = run_with_reviewer(
        Some(ApprovalDecision::Allow { note: None }),
        ApprovalPolicy::Never,
    )
    .await;
    assert!(
        asked.is_empty(),
        "nothing should have been asked: {asked:?}"
    );
    assert_eq!(statuses(&events), vec![ToolStatus::Ok, ToolStatus::Ok]);
}

/// "Always" that asks again on the next call is not a standing permission, it
/// is a slower prompt.
#[tokio::test]
async fn always_allowing_is_not_asked_a_second_time() {
    let (events, asked) = run_with_reviewer(
        Some(ApprovalDecision::AllowAlways),
        ApprovalPolicy::OnRequest,
    )
    .await;
    assert_eq!(asked, vec!["dangerous".to_string()], "asked twice");
    assert_eq!(statuses(&events), vec![ToolStatus::Ok, ToolStatus::Ok]);
}

/// A denial goes back to the model, which may work around it. An abort must
/// not: it ends the turn, with the refusal recorded so a resumed session does
/// not see a tool call that was never answered.
#[tokio::test]
async fn an_abort_ends_the_turn_with_the_refusal_recorded() {
    let (events, asked) = run_with_reviewer(
        Some(ApprovalDecision::Abort {
            reason: "not on this machine".to_string(),
        }),
        ApprovalPolicy::OnRequest,
    )
    .await;
    assert_eq!(asked, vec!["dangerous".to_string()]);
    assert_eq!(statuses(&events), vec![ToolStatus::Denied]);
    assert!(
        events.iter().any(|event| matches!(
            event,
            SessionEvent::TurnEnd {
                stop_reason: StopReason::Cancelled,
                ..
            }
        )),
        "the turn must have ended: {events:?}"
    );
}

/// Denial is monotonic: a reviewer says allow, a guard says no, and no is the
/// answer. Approval runs after the guards precisely so this cannot go the other
/// way round.
#[tokio::test]
async fn a_reviewer_cannot_undo_a_guard() {
    let harness = harness();
    let (provider, _seen) = ScriptedProvider::new(dangerous_calls());

    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.tool_contributor(Arc::new(EchoPack));
    let (reviewer, asked) = Reviewer::new(ApprovalDecision::Allow { note: None });
    extensions.approval_review_contributor(reviewer);
    extensions.tool_guard(Box::new(|call| {
        (call.name == "dangerous").then(|| "the guard says no".to_string())
    }));

    let mut session = SessionBuilder::new()
        .config(session_config_with(
            &harness.home,
            ApprovalPolicy::OnRequest,
        ))
        .provider(provider)
        .extensions(extensions.build())
        .build()
        .await
        .expect("builds");

    let log = session.log_path().to_path_buf();
    session.run_turn(Message::user("go")).await.expect("turn");
    drop(session);

    let events: Vec<SessionEvent> = read_log(&log)
        .expect("log")
        .into_iter()
        .map(|entry| entry.event)
        .collect();
    assert_eq!(
        statuses(&events),
        vec![ToolStatus::Denied, ToolStatus::Denied]
    );
    assert!(
        asked.lock().expect("lock").is_empty(),
        "a guarded call must not even reach the person"
    );
}

/// Invariant 6, cashed in: a session keke can replay is one keke can continue.
/// The second run rebuilds its history from the first run's log alone, keeps
/// writing to that same log, and sends the earlier exchange back to the model.
#[tokio::test]
async fn a_resumed_session_continues_the_log_it_was_rebuilt_from() {
    let harness = harness();

    let (provider, _seen) = ScriptedProvider::new(vec![text_reply("hello there")]);
    let mut session = SessionBuilder::new()
        .config(session_config(&harness.home))
        .provider(provider)
        .build()
        .await
        .expect("builds");
    session
        .run_turn(Message::user("hi"))
        .await
        .expect("turn completes");
    let id = session.id();
    let log = session.log_path().to_path_buf();
    drop(session);

    let resumed = keke_core::load_session(&harness.home.home, id).expect("reads the log");
    assert_eq!(resumed.history.len(), 2, "{:?}", resumed.history);
    assert_eq!(resumed.usage.total(), 15);

    let (provider, seen) = ScriptedProvider::new(vec![text_reply("still here")]);
    let mut session = SessionBuilder::new()
        .config(session_config(&harness.home))
        .provider(provider)
        .resume(id, resumed.history)
        .build()
        .await
        .expect("builds");
    session
        .run_turn(Message::user("still there?"))
        .await
        .expect("turn completes");

    // What the model saw carries the first exchange, not just the new prompt.
    let requests = seen.lock().expect("lock");
    assert_eq!(requests[0].messages.len(), 3);
    assert_eq!(requests[0].messages[0].text(), "hi");
    // And one log, not two: the resumed session appends to the file it came
    // from, or the record of the conversation is split in half.
    assert_eq!(session.log_path(), log);
}

/// Winding back to a turn drops it and everything it led to, hands the words
/// back, and leaves the next request assembled as though it never happened.
#[tokio::test]
async fn a_rewind_takes_back_the_turn_it_names() {
    let harness = harness();
    let (provider, seen) = ScriptedProvider::new(vec![
        text_reply("one"),
        text_reply("two"),
        text_reply("three"),
    ]);

    let mut session = SessionBuilder::new()
        .config(session_config(&harness.home))
        .provider(provider)
        .build()
        .await
        .expect("builds");

    session
        .run_turn(Message::user("first"))
        .await
        .expect("turn");
    session
        .run_turn(Message::user("second"))
        .await
        .expect("turn");

    let taken = session
        .rewind_to_user_turn(1, RewindScope::Conversation)
        .await
        .expect("rewinds")
        .expect("there is a second turn to take back");
    assert_eq!(taken.prompt, "second", "the words come back to be edited");
    assert_eq!(taken.removed_messages, 2);
    assert_eq!(
        session
            .history()
            .iter()
            .map(Message::text)
            .collect::<Vec<_>>(),
        vec!["first".to_string(), "one".to_string()],
        "the answer went with the question"
    );

    session
        .run_turn(Message::user("second, but better"))
        .await
        .expect("turn");
    let requests = seen.lock().expect("lock");
    let last: Vec<String> = requests[requests.len() - 1]
        .messages
        .iter()
        .map(Message::text)
        .collect();
    assert!(
        !last.iter().any(|text| text == "second"),
        "a withdrawn message must not reach the model again: {last:?}"
    );

    // Model-visible implies logged: the truncation is on disk, as a snapshot
    // of what survived it.
    let log = read_log(session.log_path()).expect("reads");
    let rewound = log
        .iter()
        .find_map(|entry| match &entry.event {
            SessionEvent::Rewound {
                history,
                prompt,
                removed_messages,
                ..
            } => Some((history.clone(), prompt.clone(), *removed_messages)),
            _ => None,
        })
        .expect("the rewind is logged");
    assert_eq!(rewound.1, "second");
    assert_eq!(rewound.2, 2);
    assert!(
        rewound.0.is_some(),
        "a conversation rewind logs the history that survived it"
    );
    assert_eq!(
        keke_core::history_from_log(&log.into_iter().map(|entry| entry.event).collect::<Vec<_>>())
            .iter()
            .map(Message::text)
            .collect::<Vec<_>>(),
        session
            .history()
            .iter()
            .map(Message::text)
            .collect::<Vec<_>>(),
        "a resumed session comes back to what the rewind left, not to what it took"
    );
}

/// Nothing to go back to is not a truncation to whatever happens to be last.
#[tokio::test]
async fn a_rewind_past_the_end_leaves_the_conversation_alone() {
    let harness = harness();
    let (provider, _seen) = ScriptedProvider::new(vec![text_reply("one")]);

    let mut session = SessionBuilder::new()
        .config(session_config(&harness.home))
        .provider(provider)
        .build()
        .await
        .expect("builds");
    session
        .run_turn(Message::user("first"))
        .await
        .expect("turn");
    let before = session.history().to_vec();

    assert_eq!(
        session
            .rewind_to_user_turn(4, RewindScope::Conversation)
            .await
            .expect("answers"),
        None
    );
    assert_eq!(session.history(), before.as_slice());
}

/// A turn that writes is snapshotted before it does, and the snapshot is what
/// a files rewind puts back.
fn writes(id: &str, path: &str, text: &str) -> Vec<StreamChunk> {
    let call = ToolCallId::new(id);
    vec![
        StreamChunk::ToolCallStart {
            id: call.clone(),
            name: "write_file".to_string(),
        },
        StreamChunk::ToolCallArgsDelta {
            id: call.clone(),
            delta: serde_json::json!({ "path": path, "text": text }).to_string(),
        },
        StreamChunk::ToolCallEnd { id: call },
        StreamChunk::Done(StopReason::ToolUse),
    ]
}

/// The other half of a rewind: what the turn did to the files goes back too,
/// and the conversation can be left alone while it happens.
#[tokio::test]
async fn a_files_rewind_puts_the_working_tree_back_without_touching_the_talk() {
    let harness = harness();
    let (provider, _seen) = ScriptedProvider::new(vec![
        writes("call-1", "file.txt", "first"),
        text_reply("wrote it"),
        writes("call-2", "file.txt", "second"),
        text_reply("wrote it again"),
    ]);
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.tool_contributor(Arc::new(EchoPack));

    let mut session = SessionBuilder::new()
        .config(session_config(&harness.home))
        .provider(provider)
        .extensions(extensions.build())
        .build()
        .await
        .expect("builds");

    let file = harness.home.workspace_root.as_path().join("file.txt");
    session
        .run_turn(Message::user("write first"))
        .await
        .expect("turn");
    assert_eq!(std::fs::read_to_string(&file).expect("written"), "first");
    session
        .run_turn(Message::user("write second"))
        .await
        .expect("turn");
    assert_eq!(std::fs::read_to_string(&file).expect("written"), "second");

    // Every turn that wrote is a point the files can go back to.
    let points = session.rewind_points();
    assert!(
        points.iter().all(|point| point.has_snapshot),
        "both turns wrote, so both were snapshotted first: {points:?}"
    );

    let rewound = session
        .rewind_to_user_turn(1, RewindScope::Files)
        .await
        .expect("rewinds")
        .expect("there is a second turn");

    assert_eq!(
        std::fs::read_to_string(&file).expect("still there"),
        "first",
        "the second turn's edit is undone"
    );
    assert_eq!(rewound.restored_files, vec!["file.txt".to_string()]);
    assert_eq!(
        rewound.removed_messages, 0,
        "files only: what was said is left alone"
    );
    assert_eq!(
        session
            .history()
            .iter()
            .filter(|message| message.role == keke_protocol::Role::User)
            .count(),
        2,
        "both prompts are still in the conversation"
    );
}

/// Winding both back is the third choice, and it is the two halves together.
#[tokio::test]
async fn rewinding_both_takes_back_the_words_and_the_edit() {
    let harness = harness();
    let (provider, _seen) = ScriptedProvider::new(vec![
        writes("call-1", "file.txt", "first"),
        text_reply("wrote it"),
        writes("call-2", "file.txt", "second"),
        text_reply("wrote it again"),
    ]);
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.tool_contributor(Arc::new(EchoPack));

    let mut session = SessionBuilder::new()
        .config(session_config(&harness.home))
        .provider(provider)
        .extensions(extensions.build())
        .build()
        .await
        .expect("builds");

    let file = harness.home.workspace_root.as_path().join("file.txt");
    session
        .run_turn(Message::user("write first"))
        .await
        .expect("turn");
    session
        .run_turn(Message::user("write second"))
        .await
        .expect("turn");

    let rewound = session
        .rewind_to_user_turn(1, RewindScope::Both)
        .await
        .expect("rewinds")
        .expect("there is a second turn");

    assert_eq!(rewound.prompt, "write second");
    assert_eq!(
        std::fs::read_to_string(&file).expect("still there"),
        "first"
    );
    assert_eq!(
        session
            .history()
            .iter()
            .filter(|message| message.role == keke_protocol::Role::User)
            .count(),
        1,
        "the second prompt went with its edit"
    );
    assert!(
        !session.rewind_points().iter().any(|point| point.turn == 1),
        "a turn that was wound back is no longer somewhere to go back to"
    );
}

/// A turn that only talked has nothing on disk to put back, and says so rather
/// than offering a restore that would do nothing.
#[tokio::test]
async fn a_turn_that_wrote_nothing_carries_no_snapshot() {
    let harness = harness();
    let (provider, _seen) = ScriptedProvider::new(vec![text_reply("just talking")]);

    let mut session = SessionBuilder::new()
        .config(session_config(&harness.home))
        .provider(provider)
        .build()
        .await
        .expect("builds");
    session
        .run_turn(Message::user("hello"))
        .await
        .expect("turn");

    let points = session.rewind_points();
    assert_eq!(points.len(), 1);
    assert!(
        !points[0].has_snapshot,
        "nothing was written, so there is no point snapshotting the tree"
    );
    assert!(
        session
            .changed_since_turn(0)
            .await
            .expect("answers")
            .is_empty()
    );
}
