//! The turn loop.
//!
//! One turn is one user input plus everything the agent does in response: model
//! call, tool calls, model call again, until the model stops asking for tools or
//! a limit is hit.
//!
//! Every model-visible input is logged before it is sent, not after it returns.
//! Logging after would lose the request that crashed the process, which is
//! exactly the one worth having.

use std::sync::Arc;

use futures::StreamExt;
use keke_plugin_api::ExtensionContext;
use keke_protocol::ContentBlock;
use keke_protocol::Message;
use keke_protocol::Role;
use keke_protocol::SessionEvent;
use keke_protocol::StopReason;
use keke_protocol::ToolCall;
use keke_protocol::ToolCallId;
use keke_protocol::TurnId;
use keke_protocol::Usage;
use keke_provider_api::ModelRequest;
use keke_provider_api::ProviderError;
use keke_provider_api::StreamChunk;
use keke_provider_api::ToolSpec;
use keke_tool::ListToolsContext;

use crate::CoreError;
use crate::Session;
use crate::TurnUpdate;
use crate::dispatch::Dispatch;
use crate::dispatch::ToolSet;
use crate::dispatch::dispatch;

/// How many model calls one turn may make before giving up.
///
/// A model that keeps calling tools without converging would otherwise run
/// forever. This is a safety stop, not a budget: hitting it is a bug worth
/// surfacing, and it is reported as such.
const MAX_STEPS_PER_TURN: usize = 64;

/// What one turn produced.
#[derive(Clone, Debug)]
pub struct TurnOutcome {
    pub turn: TurnId,
    pub stop_reason: StopReason,
    pub usage: Usage,
    /// The assistant's final message, when it produced one.
    pub message: Option<Message>,
}

impl Session {
    /// Run one turn to completion.
    pub async fn run_turn(&mut self, input: Message) -> Result<TurnOutcome, CoreError> {
        // A cancel belongs to the turn it interrupted. Carrying it forward made
        // the next turn stop after its first tool batch, which reads as the
        // agent giving up for no reason — and only when that turn used a tool,
        // since that is the one place the flag is read.
        self.reset_cancellation();

        let turn = TurnId::new();
        let ext_ctx = ExtensionContext::new(self.id, self.thread).in_turn(turn);

        self.log(SessionEvent::TurnStart {
            turn,
            input: input.clone(),
            approval_policy: Some(self.approval.get().as_str().to_string()),
        })
        .await?;
        self.emit(TurnUpdate::TurnStarted { turn });

        for contributor in self.registry.turn_lifecycle_contributors() {
            contributor.on_turn_start(&ext_ctx, turn).await;
        }

        let result = self.run_steps(turn, input, &ext_ctx).await;

        let outcome = match result {
            Ok(outcome) => outcome,
            Err(error) => {
                self.log(SessionEvent::Error {
                    turn: Some(turn),
                    message: error.to_string(),
                })
                .await?;
                for contributor in self.registry.turn_lifecycle_contributors() {
                    contributor
                        .on_turn_end(&ext_ctx, turn, &StopReason::Cancelled)
                        .await;
                }
                return Err(error);
            }
        };

        self.log(SessionEvent::TurnEnd {
            turn,
            stop_reason: outcome.stop_reason.clone(),
            usage: outcome.usage,
        })
        .await?;
        for contributor in self.registry.turn_lifecycle_contributors() {
            contributor
                .on_turn_end(&ext_ctx, turn, &outcome.stop_reason)
                .await;
        }
        // Anything a lifecycle contributor recorded on its way out.
        for event in ext_ctx.drain_events() {
            self.log(event).await?;
        }
        self.emit(TurnUpdate::TurnEnded {
            turn,
            stop_reason: outcome.stop_reason.clone(),
        });

        Ok(outcome)
    }

    async fn run_steps(
        &mut self,
        turn: TurnId,
        input: Message,
        ext_ctx: &ExtensionContext,
    ) -> Result<TurnOutcome, CoreError> {
        self.history.push(input);

        // Before the turn, not during it: compacting mid-turn would drop the
        // tool results the model is in the middle of reasoning about.
        self.compact_if_needed(turn).await?;

        let tools = ToolSet::from_registry(&self.registry, ext_ctx);
        let system = crate::prompt::assemble_system_prompt(
            &self.workspace,
            &self.cwd,
            &self.registry,
            ext_ctx,
        )
        .await;
        // Before the first `ModelRequest`, so a fragment is on disk ahead of
        // the request it is part of rather than trailing the first tool call.
        for event in ext_ctx.drain_events() {
            self.log(event).await?;
        }
        let specs = tool_specs(&tools);
        let tool_names: Vec<String> = specs.iter().map(|spec| spec.name.clone()).collect();

        let mut usage = Usage::default();
        // What the last logged `ModelRequest` said, so a step that changed
        // neither the model nor the reasoning effort doesn't log again.
        // `tools` never varies within a turn (`specs` is fixed above the
        // loop), so it is not part of this comparison.
        let mut last_logged: Option<(Option<keke_protocol::ReasoningEffort>, String)> = None;

        for step in 0..MAX_STEPS_PER_TURN {
            let request = ModelRequest {
                model: self.model.get().to_string(),
                system: Some(system.clone()),
                messages: self.history.clone(),
                tools: specs.clone(),
                // Left to the provider: the engine advertises the tools it can
                // run, and a tool the vendor runs for itself is the vendor's to
                // add.
                hosted_tools: Vec::new(),
                vendor_params: serde_json::Map::new(),
                max_output_tokens: Some(self.config.max_output_tokens.get()),
                temperature: None,
                reasoning_effort: self.effort.get(),
            };

            // Logged before the call, so a crash mid-request still leaves the
            // request that caused it on disk. Only the first step of a turn
            // carries the full `messages` snapshot: later steps within the
            // same turn only append a `ModelResponse` and `ToolCallEnd`s onto
            // it, and `history_from_log` already replays those onto the last
            // snapshot it found. Logging the whole history again on every
            // step made the log grow quadratically in the number of steps for
            // no reconstructive benefit.
            //
            // Beyond the first step, a step is logged at all only when the
            // model or the reasoning effort actually changed since the last
            // logged step — both can be switched mid-turn from outside this
            // loop, which is genuinely model-visible and must be logged per
            // invariant 6, but a step that repeats the same values the model
            // already saw has nothing new to record.
            let current = (request.reasoning_effort, request.model.clone());
            if step == 0 || last_logged.as_ref() != Some(&current) {
                self.log(SessionEvent::ModelRequest {
                    turn,
                    messages: if step == 0 {
                        request.messages.clone()
                    } else {
                        Vec::new()
                    },
                    tools: tool_names.clone(),
                    reasoning_effort: request.reasoning_effort,
                    model: Some(request.model.clone()),
                })
                .await?;
                last_logged = Some(current);
            }

            let (message, stop_reason, step_usage) = self.stream_one_step(turn, request).await?;
            usage.add(step_usage);
            self.emit(TurnUpdate::StepUsage {
                turn,
                usage: step_usage,
            });

            self.log(SessionEvent::ModelResponse {
                turn,
                message: message.clone(),
                stop_reason: stop_reason.clone(),
                usage: step_usage,
            })
            .await?;
            self.history.push(message.clone());

            let calls = tool_calls(&message);
            if calls.is_empty() {
                return Ok(TurnOutcome {
                    turn,
                    stop_reason,
                    usage,
                    message: Some(message),
                });
            }

            let mut results = Vec::with_capacity(calls.len());
            let mut aborted = false;
            for call in &calls {
                self.log(SessionEvent::ToolCallStart {
                    turn,
                    call: call.clone(),
                })
                .await?;
                self.emit(TurnUpdate::ToolCallStarted { call: call.clone() });

                let dispatched = dispatch(
                    call,
                    Dispatch {
                        tools: &tools,
                        registry: &self.registry,
                        ext_ctx,
                        workspace_root: self.workspace.root(),
                        cancelled: Arc::clone(&self.cancelled),
                        policy: self.approval.get(),
                        memory: &self.approvals,
                    },
                )
                .await;
                let result = dispatched.result;
                aborted |= dispatched.abort;

                // Drained before the call's own end event so an extension's
                // record of what it put in front of a model precedes the
                // result that reported it (`AGENTS.md` invariant 6).
                for event in ext_ctx.drain_events() {
                    self.log(event).await?;
                }
                self.log(SessionEvent::ToolCallEnd {
                    turn,
                    result: result.clone(),
                })
                .await?;
                self.emit(TurnUpdate::ToolCallEnded {
                    result: result.clone(),
                });
                results.push(ContentBlock::ToolResult(result));
            }

            self.history.push(Message {
                role: Role::Tool,
                content: results,
            });

            // An abort still records its results first: the model's request and
            // the refusal it earned both belong in the history, or a resumed
            // session sees a tool call that was never answered.
            if aborted || self.is_cancelled() {
                return Ok(TurnOutcome {
                    turn,
                    stop_reason: StopReason::Cancelled,
                    usage,
                    message: Some(message),
                });
            }
        }

        Err(CoreError::StepLimit {
            steps: MAX_STEPS_PER_TURN,
        })
    }

    /// Summarize the older history when it has outgrown its budget.
    ///
    /// A failed summarization is not fatal: the turn proceeds uncompacted and
    /// will probably be rejected by the provider, which is a clearer failure
    /// than refusing to answer at all. The attempt is logged either way.
    async fn compact_if_needed(&mut self, turn: TurnId) -> Result<(), CoreError> {
        if !crate::compact::should_compact(&self.history, &self.config.compaction) {
            return Ok(());
        }
        let Some((older, recent)) =
            crate::compact::split_for_compaction(&self.history, &self.config.compaction)
        else {
            return Ok(());
        };

        let removed = older.len();
        let mut messages = older.to_vec();
        messages.push(Message::user(crate::compact::SUMMARY_INSTRUCTION));
        let recent = recent.to_vec();

        let request = ModelRequest {
            model: self.model.get().to_string(),
            system: None,
            messages,
            tools: Vec::new(),
            hosted_tools: Vec::new(),
            vendor_params: serde_json::Map::new(),
            max_output_tokens: Some(self.config.max_output_tokens.get()),
            temperature: None,
            // Summarizing is keke's own errand, not the user's turn, and it is
            // extractive work: paying for extended thinking on it would change
            // the bill without changing the summary.
            reasoning_effort: None,
        };

        let summary = match self.collect_text(request).await {
            Ok(text) if !text.trim().is_empty() => text,
            Ok(_) | Err(_) => {
                tracing::warn!("compaction produced no summary; continuing uncompacted");
                return Ok(());
            }
        };

        let summary = crate::compact::summary_message(&summary);
        self.history = std::iter::once(summary.clone()).chain(recent).collect();

        self.log(SessionEvent::Compacted {
            turn,
            summary,
            removed_messages: removed,
        })
        .await
    }

    /// Run one model call for keke's own purposes and return just its text.
    async fn collect_text(&self, request: ModelRequest) -> Result<String, CoreError> {
        let mut stream = self.stream_with_reauth(request).await?;
        let mut text = String::new();
        while let Some(chunk) = stream.next().await {
            if let StreamChunk::TextDelta(delta) = chunk? {
                text.push_str(&delta);
            }
        }
        Ok(text)
    }

    /// Drive the provider stream for one step, assembling a message from chunks.
    ///
    /// Takes the already-built request rather than rebuilding it, so the
    /// request that was logged is exactly the request that is sent. Rebuilding
    /// here would let the two drift, and a replay would then diverge from the
    /// live run for reasons invisible in the log.
    async fn stream_one_step(
        &mut self,
        turn: TurnId,
        request: ModelRequest,
    ) -> Result<(Message, StopReason, Usage), CoreError> {
        let mut stream = self.stream_with_reauth(request).await?;
        let mut assembler = MessageAssembler::default();

        while let Some(chunk) = stream.next().await {
            // Checked per chunk, not just between steps: a cancel raised while
            // the model is still streaming text has no tool dispatch to catch
            // it at, and would otherwise run the stream to completion in the
            // background while the UI already reads as idle.
            if self.is_cancelled() {
                return Ok((assembler.finish(), StopReason::Cancelled, assembler.usage));
            }
            match chunk? {
                StreamChunk::TextDelta(delta) => {
                    self.emit(TurnUpdate::TextDelta {
                        turn,
                        delta: delta.clone(),
                    });
                    assembler.text.push_str(&delta);
                }
                StreamChunk::ThinkingDelta(delta) => {
                    self.emit(TurnUpdate::ThinkingDelta {
                        turn,
                        delta: delta.clone(),
                    });
                    assembler.thinking.push_str(&delta);
                }
                StreamChunk::ThinkingSignature(signature) => {
                    assembler.thinking_signature = Some(signature);
                }
                StreamChunk::ToolCallStart { id, name } => {
                    assembler.calls.push((id, name, String::new()));
                }
                StreamChunk::ToolCallArgsDelta { id, delta } => {
                    if let Some(entry) = assembler.calls.iter_mut().find(|(call, _, _)| call == &id)
                    {
                        entry.2.push_str(&delta);
                    }
                }
                StreamChunk::ToolCallEnd { .. } => {}
                StreamChunk::HostedToolCall { name, query } => {
                    // Not fed into `assembler.calls`: that path assembles a
                    // `ContentBlock::ToolCall` the turn loop later dispatches
                    // against the local tool registry, and a hosted tool has
                    // no entry there — dispatching it would report the
                    // vendor's own search back to the model as an unknown
                    // tool. Logged directly instead, so it is still on the
                    // record (invariant 6) without going through dispatch.
                    self.emit(TurnUpdate::HostedToolCall {
                        turn,
                        name: name.clone(),
                        query: query.clone(),
                    });
                    self.log(SessionEvent::HostedToolCall { turn, name, query })
                        .await?;
                }
                StreamChunk::Usage(usage) => assembler.usage = usage,
                StreamChunk::Done(reason) => {
                    return Ok((assembler.finish(), reason, assembler.usage));
                }
            }
        }

        // The provider contract says a successful stream ends with `Done`.
        Err(CoreError::Provider(ProviderError::Protocol(
            "the provider stream ended without a terminal chunk".to_string(),
        )))
    }

    /// Call the provider, refreshing credentials and retrying **once** on a 401.
    ///
    /// Once, not repeatedly: a credential that fails immediately after a
    /// successful refresh is not going to start working on the third attempt,
    /// and retrying would turn a clear auth error into a hang.
    async fn stream_with_reauth(
        &self,
        request: ModelRequest,
    ) -> Result<keke_provider_api::StreamEvent, CoreError> {
        match self.provider.stream(request.clone()).await {
            Err(error) if error.needs_reauth() => {
                let refreshed = match &self.auth {
                    Some(auth) => auth.refresh_after_unauthorized().await,
                    None => false,
                };
                if !refreshed {
                    return Err(CoreError::Provider(error));
                }
                self.provider
                    .stream(request)
                    .await
                    .map_err(CoreError::Provider)
            }
            other => other.map_err(CoreError::Provider),
        }
    }
}

/// Accumulates streamed chunks into one assistant message.
#[derive(Default)]
struct MessageAssembler {
    thinking: String,
    /// Opaque, provider-issued, and replayed unchanged: some wires reject a
    /// thinking block that comes back without the signature they minted.
    thinking_signature: Option<String>,
    text: String,
    calls: Vec<(ToolCallId, String, String)>,
    usage: Usage,
}

impl MessageAssembler {
    fn finish(&self) -> Message {
        let mut content = Vec::new();
        if !self.thinking.is_empty() {
            content.push(ContentBlock::Thinking {
                text: self.thinking.clone(),
                signature: self.thinking_signature.clone(),
            });
        }
        if !self.text.is_empty() {
            content.push(ContentBlock::text(&self.text));
        }
        for (id, name, arguments) in &self.calls {
            // Arguments arrive as text fragments; a model that emits invalid
            // JSON is reported to itself as a tool error rather than crashing
            // the turn, so parse leniently here and let dispatch reject it.
            let parsed = if arguments.trim().is_empty() {
                serde_json::Value::Object(serde_json::Map::new())
            } else {
                serde_json::from_str(arguments)
                    .unwrap_or_else(|_| serde_json::Value::String(arguments.clone()))
            };
            content.push(ContentBlock::ToolCall(ToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: parsed,
            }));
        }

        Message {
            role: Role::Assistant,
            content,
        }
    }
}

/// Extract the tool calls a message is asking for.
fn tool_calls(message: &Message) -> Vec<ToolCall> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .collect()
}

/// Advertise the tool set to the model.
fn tool_specs(tools: &ToolSet) -> Vec<ToolSpec> {
    let siblings: Vec<_> = tools.iter().map(|tool| tool.id()).collect();
    let ctx = ListToolsContext {
        siblings,
        attributes: Default::default(),
    };

    tools
        .iter()
        .filter(|tool| tool.should_list(&ctx))
        .map(|tool| ToolSpec {
            name: tool.id().to_string(),
            description: tool.description(&ctx).text,
            input_schema: tool.input_schema(),
        })
        .collect()
}
