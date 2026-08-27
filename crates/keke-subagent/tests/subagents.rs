//! End-to-end subagent tests against a scripted provider.
//!
//! Every assertion here is one of the claims in this crate's module docs. The
//! one that matters most is the first: a subagent cannot start a subagent, and
//! that has to be true of what the *model actually saw*, not of a counter kept
//! somewhere it can be reasoned about instead of checked.

// An integration test is not `#[cfg(test)]`, so the workspace's
// allow-expect-in-tests setting does not reach it. A panic is the assertion.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use futures::StreamExt;
use keke_config_types::ApprovalPolicy;
use keke_config_types::CompactionConfig;
use keke_config_types::HomeLayout;
use keke_config_types::MaxOutputTokens;
use keke_config_types::ModelSelection;
use keke_config_types::SubagentLimits;
use keke_core::SessionBuilder;
use keke_core::read_log;
use keke_paths::AbsPath;
use keke_plugin_api::ExtensionRegistryBuilder;
use keke_protocol::ContentBlock;
use keke_protocol::Message;
use keke_protocol::Role;
use keke_protocol::SessionEvent;
use keke_protocol::StopReason;
use keke_protocol::ToolCallId;
use keke_protocol::Usage;
use keke_provider_api::ModelProvider;
use keke_provider_api::ModelRequest;
use keke_provider_api::ProviderError;
use keke_provider_api::ProviderFuture;
use keke_provider_api::ProviderInfo;
use keke_provider_api::StreamChunk;
use keke_provider_api::StreamEvent;
use keke_provider_api::WireApi;

// ---------------------------------------------------------------- provider

/// Replies by looking at what it was asked, not by position in a script.
///
/// Parent and child call the same provider concurrently, so a positional script
/// would make these tests depend on which of two sessions got there first.
struct ScriptedProvider {
    info: ProviderInfo,
    seen: Arc<Mutex<Vec<ModelRequest>>>,
    /// The highest number of calls in flight at once, which is how the
    /// concurrency bound is observed rather than assumed.
    in_flight: AtomicUsize,
    peak: AtomicUsize,
    /// Held while streaming, so overlapping calls actually overlap.
    dwell: std::time::Duration,
}

impl ScriptedProvider {
    fn new(dwell_millis: u64) -> (Arc<Self>, Arc<Mutex<Vec<ModelRequest>>>) {
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
            seen: Arc::clone(&seen),
            in_flight: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            dwell: std::time::Duration::from_millis(dwell_millis),
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
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(self.dwell).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);

            let last = request.messages.last().cloned();
            self.seen.lock().expect("lock").push(request);

            let chunks = match last {
                // The tool results came back: wrap the turn up.
                Some(message) if message.role == Role::Tool => text_reply("done"),
                // A subagent's opening prompt is its task, verbatim.
                Some(message) if message.text().starts_with("SUB") => {
                    text_reply(&format!("finished: {}", message.text()))
                }
                // The parent's opening prompt: delegate.
                Some(message) => spawn_reply(&message.text()),
                None => text_reply("nothing to do"),
            };
            Ok(futures::stream::iter(chunks.into_iter().map(Ok)).boxed())
        })
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

/// A parent prompt of `spawn:N` asks for N subagents, none of them waited on;
/// anything else asks for one and waits.
fn spawn_reply(prompt: &str) -> Vec<StreamChunk> {
    let (count, wait) = match prompt.strip_prefix("spawn:") {
        Some(n) => (n.parse().unwrap_or(1), false),
        None => (1, true),
    };

    let mut chunks = Vec::new();
    for index in 0..count {
        let id = ToolCallId::new(format!("call-{index}"));
        chunks.push(StreamChunk::ToolCallStart {
            id: id.clone(),
            name: "spawn_agent".to_string(),
        });
        chunks.push(StreamChunk::ToolCallArgsDelta {
            id: id.clone(),
            delta: format!("{{\"task\":\"SUB {index}\",\"wait\":{wait}}}"),
        });
        chunks.push(StreamChunk::ToolCallEnd { id });
    }
    chunks.push(StreamChunk::Usage(Usage {
        input_tokens: 10,
        output_tokens: 5,
        ..Usage::default()
    }));
    chunks.push(StreamChunk::Done(StopReason::ToolUse));
    chunks
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
    keke_core::SessionConfig {
        model: ModelSelection {
            provider: "scripted".to_string(),
            model: "test-model".to_string(),
        },
        home: home.clone(),
        max_output_tokens: MaxOutputTokens::default(),
        reasoning_effort: None,
        compaction: CompactionConfig::default(),
        approval: ApprovalPolicy::Never,
    }
}

/// Compose a session the way `keke-cli` does: install, build the recipe, hand
/// the recipe back to the host.
async fn parent(
    harness: &Harness,
    provider: Arc<ScriptedProvider>,
    limits: SubagentLimits,
    attach: bool,
) -> keke_core::Session {
    let mut extensions = ExtensionRegistryBuilder::new();
    let host = keke_subagent::install(&mut extensions, limits);

    let builder = SessionBuilder::new()
        .config(session_config(&harness.home))
        .provider(provider)
        .extensions(extensions.build());

    if attach {
        host.attach(builder.clone());
    }
    builder.build().await.expect("builds")
}

/// The tool names offered in each model request, in order.
fn advertised(seen: &Arc<Mutex<Vec<ModelRequest>>>) -> Vec<Vec<String>> {
    seen.lock()
        .expect("lock")
        .iter()
        .map(|request| {
            request
                .tools
                .iter()
                .map(|spec| spec.name.clone())
                .collect::<Vec<_>>()
        })
        .collect()
}

// ---------------------------------------------------------------- tests

#[tokio::test]
async fn a_subagent_is_not_offered_the_tools_that_made_it() {
    let harness = harness();
    let (provider, seen) = ScriptedProvider::new(0);
    let mut session = parent(&harness, provider, SubagentLimits::default(), true).await;

    session
        .run_turn(Message::user("delegate this"))
        .await
        .expect("turn completes");

    let requests = advertised(&seen);
    // The parent's first request offered them; at least one request did not,
    // and that one is the child's. A depth counter would be checkable only in
    // this crate — what the model was advertised is checkable from outside it.
    let parent_saw: Vec<_> = requests
        .iter()
        .filter(|tools| tools.iter().any(|name| name == "spawn_agent"))
        .collect();
    let child_saw: Vec<_> = requests
        .iter()
        .filter(|tools| !tools.iter().any(|name| name == "spawn_agent"))
        .collect();

    assert!(!parent_saw.is_empty(), "the parent was never offered them");
    assert_eq!(child_saw.len(), 1, "exactly one request was a subagent's");
    assert!(
        !child_saw[0].iter().any(|name| name == "collect_agent"),
        "the subagent could still collect: {child_saw:?}"
    );
}

#[tokio::test]
async fn a_subagents_work_is_recorded_in_the_parents_log() {
    let harness = harness();
    let (provider, _seen) = ScriptedProvider::new(0);
    let mut session = parent(&harness, provider, SubagentLimits::default(), true).await;

    session
        .run_turn(Message::user("delegate this"))
        .await
        .expect("turn completes");

    let log = read_log(session.log_path()).expect("reads");
    let started = log.iter().find_map(|entry| match &entry.event {
        SessionEvent::SubagentStart { agent, task, .. } => Some((agent.clone(), task.clone())),
        _ => None,
    });
    let ended = log.iter().find_map(|entry| match &entry.event {
        SessionEvent::SubagentEnd {
            agent,
            session,
            status,
            summary,
            ..
        } => Some((agent.clone(), *session, status.clone(), summary.clone())),
        _ => None,
    });

    let (started_agent, task) = started.expect("the parent's log names the subagent it started");
    let (ended_agent, child_session, status, summary) =
        ended.expect("the parent's log says how the subagent ended");

    assert_eq!(started_agent, ended_agent);
    assert_eq!(task, "SUB 0");
    assert_eq!(status, "completed");
    assert_eq!(summary, "finished: SUB 0");
    // Without this the child's own log could not be found from the parent's.
    assert!(child_session.is_some(), "the child's session is unnamed");
    assert_ne!(child_session, Some(session.id()));
}

#[tokio::test]
async fn the_pool_bounds_how_many_subagents_run_at_once() {
    let harness = harness();
    // Each model call dwells, so two subagents allowed to overlap would.
    let (provider, _seen) = ScriptedProvider::new(40);
    let gauge = Arc::clone(&provider);
    let mut session = parent(
        &harness,
        provider,
        SubagentLimits {
            max_concurrent: 1,
            ..SubagentLimits::default()
        },
        true,
    )
    .await;

    session
        .run_turn(Message::user("spawn:3"))
        .await
        .expect("turn completes");
    // The turn returns once the parent stops calling tools; the subagents it
    // never collected are still outstanding, so drain them before measuring.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    // One is the parent's own call. Two at once would mean the permit did
    // nothing, which is exactly the failure a queued spawn is bounded against.
    assert!(
        gauge.peak.load(Ordering::SeqCst) <= 2,
        "peak concurrency was {}",
        gauge.peak.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn subagent_tools_are_withheld_until_a_recipe_is_attached() {
    let harness = harness();
    let (provider, seen) = ScriptedProvider::new(0);
    // No attach: a `spawn_agent` that cannot build a session must not be
    // advertised at all rather than fail once the model has committed to it.
    let mut session = parent(&harness, provider, SubagentLimits::default(), false).await;

    session
        .run_turn(Message::user("delegate this"))
        .await
        .expect("turn completes");

    for tools in advertised(&seen) {
        assert!(
            !tools.iter().any(|name| name == "spawn_agent"),
            "an unattached host still advertised: {tools:?}"
        );
    }
}

#[tokio::test]
async fn a_subagent_is_reported_once_and_then_is_gone() {
    let harness = harness();
    let (provider, _seen) = ScriptedProvider::new(0);
    let mut extensions = ExtensionRegistryBuilder::new();
    let host = keke_subagent::install(&mut extensions, SubagentLimits::default());
    let builder = SessionBuilder::new()
        .config(session_config(&harness.home))
        .provider(provider)
        .extensions(extensions.build());
    host.attach(builder);

    let cancelled: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(|| false);
    let id = host
        .spawn("SUB once".to_string(), cancelled)
        .expect("spawns");

    let report = host.collect(&id).await.expect("collects");
    assert_eq!(report.summary, "finished: SUB once");
    assert!(host.outstanding().is_empty());

    // A second collect names the handle that no longer exists rather than
    // replaying an answer the model already has.
    assert!(matches!(
        host.collect(&id).await,
        Err(keke_subagent::SubagentError::Unknown(_))
    ));
}

#[tokio::test]
async fn collecting_a_subagent_that_was_never_started_names_it() {
    let harness = harness();
    let (provider, _seen) = ScriptedProvider::new(0);
    let mut extensions = ExtensionRegistryBuilder::new();
    let host = keke_subagent::install(&mut extensions, SubagentLimits::default());
    let builder = SessionBuilder::new()
        .config(session_config(&harness.home))
        .provider(provider)
        .extensions(extensions.build());
    host.attach(builder);

    // Nothing was spawned, so there is nothing to collect — and saying so is
    // not the same as reporting success with no agents.
    assert!(host.outstanding().is_empty());
    assert!(matches!(
        host.collect("agent_9").await,
        Err(keke_subagent::SubagentError::Unknown(_))
    ));
}

/// The report the parent's model reads must carry what the child said, since a
/// subagent whose answer does not reach the parent has cost tokens for nothing.
#[tokio::test]
async fn the_parents_model_is_told_what_the_subagent_found() {
    let harness = harness();
    let (provider, seen) = ScriptedProvider::new(0);
    let mut session = parent(&harness, provider, SubagentLimits::default(), true).await;

    session
        .run_turn(Message::user("delegate this"))
        .await
        .expect("turn completes");

    let requests = seen.lock().expect("lock");
    let carried = requests.iter().any(|request| {
        request.messages.iter().any(|message| {
            message.role == Role::Tool
                && message.content.iter().any(|block| match block {
                    ContentBlock::ToolResult(result) => result
                        .content
                        .iter()
                        .any(|block| matches!(block, ContentBlock::Text { text } if text.contains("finished: SUB 0"))),
                    _ => false,
                })
        })
    });
    assert!(carried, "the subagent's answer never reached the parent");
}
