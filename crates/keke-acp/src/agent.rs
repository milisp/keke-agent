//! keke as an ACP agent, spoken over stdio.
//!
//! The editor is the client and keke is the agent: it receives prompts and
//! emits session notifications. Nothing here reaches into the engine — it
//! drives a [`Conversation`], the same thing the terminal interface drives, so
//! the two surfaces cannot drift into different behaviour.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use agent_client_protocol::Agent;
use agent_client_protocol::ConnectionTo;
use agent_client_protocol::Stdio;
use agent_client_protocol::schema::v1::AgentCapabilities;
use agent_client_protocol::schema::v1::CancelNotification;
use agent_client_protocol::schema::v1::ContentBlock;
use agent_client_protocol::schema::v1::ContentChunk;
use agent_client_protocol::schema::v1::Implementation;
use agent_client_protocol::schema::v1::InitializeRequest;
use agent_client_protocol::schema::v1::InitializeResponse;
use agent_client_protocol::schema::v1::NewSessionRequest;
use agent_client_protocol::schema::v1::NewSessionResponse;
use agent_client_protocol::schema::v1::PermissionOption;
use agent_client_protocol::schema::v1::PermissionOptionKind;
use agent_client_protocol::schema::v1::PromptRequest;
use agent_client_protocol::schema::v1::PromptResponse;
use agent_client_protocol::schema::v1::RequestPermissionOutcome;
use agent_client_protocol::schema::v1::RequestPermissionRequest;
use agent_client_protocol::schema::v1::SessionId;
use agent_client_protocol::schema::v1::SessionNotification;
use agent_client_protocol::schema::v1::SessionUpdate;
use agent_client_protocol::schema::v1::StopReason as AcpStopReason;
use agent_client_protocol::schema::v1::TextContent;
use agent_client_protocol::schema::v1::ToolCallStatus;
use agent_client_protocol::schema::v1::ToolCallUpdate;
use agent_client_protocol::schema::v1::ToolCallUpdateFields;
use keke_protocol::StopReason;
use keke_protocol::ToolStatus;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;

use crate::Conversation;
use crate::ConversationError;
use crate::ConversationFuture;
use crate::PermissionAnswer;
use crate::Update;

/// Makes a conversation for one ACP session.
///
/// A trait rather than a closure because the composition root is the only place
/// that knows how to build a session, and `keke-acp` must not learn.
pub trait SessionFactory: Send + Sync + 'static {
    /// Open a conversation rooted at `cwd`, as the client asked.
    fn open(
        &self,
        cwd: PathBuf,
    ) -> ConversationFuture<
        '_,
        Result<(Arc<dyn Conversation>, UnboundedReceiver<Update>), ConversationError>,
    >;
}

/// The identifiers the option ids are built from.
///
/// The ACP client sends back an option id, so these strings are the wire
/// contract for what a person chose.
const ALLOW: &str = "allow";
const ALLOW_ALWAYS: &str = "allow-always";
const DENY: &str = "deny";

fn permission_options() -> Vec<PermissionOption> {
    vec![
        PermissionOption::new(ALLOW, "Allow once", PermissionOptionKind::AllowOnce),
        PermissionOption::new(
            ALLOW_ALWAYS,
            "Allow for the rest of this session",
            PermissionOptionKind::AllowAlways,
        ),
        PermissionOption::new(DENY, "Deny", PermissionOptionKind::RejectOnce),
    ]
}

/// One live ACP session.
struct Entry {
    conversation: Arc<dyn Conversation>,
    /// Fed by the pump when a turn ends, read by the prompt handler. The
    /// stop reason arrives on the update stream, but the client wants it as the
    /// response to `session/prompt`.
    outcomes: tokio::sync::Mutex<UnboundedReceiver<AcpStopReason>>,
}

#[derive(Default)]
struct Sessions(Mutex<HashMap<String, Arc<Entry>>>);

impl Sessions {
    fn get(&self, id: &SessionId) -> Option<Arc<Entry>> {
        self.0.lock().ok()?.get(id.0.as_ref()).cloned()
    }

    fn insert(&self, id: &SessionId, entry: Arc<Entry>) {
        if let Ok(mut sessions) = self.0.lock() {
            sessions.insert(id.to_string(), entry);
        }
    }
}

/// Serve the ACP protocol on stdin and stdout until the client disconnects.
pub async fn serve_stdio(
    factory: Arc<dyn SessionFactory>,
) -> Result<(), agent_client_protocol::Error> {
    let sessions = Arc::new(Sessions::default());
    let next = Arc::new(std::sync::atomic::AtomicU64::new(1));

    Agent
        .builder()
        .name("keke")
        .on_receive_request(
            async move |request: InitializeRequest, responder, _cx| {
                responder.respond(
                    InitializeResponse::new(request.protocol_version)
                        .agent_capabilities(AgentCapabilities::new())
                        .agent_info(Implementation::new("keke", env!("CARGO_PKG_VERSION"))),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                let factory = Arc::clone(&factory);
                let next = Arc::clone(&next);
                async move |request: NewSessionRequest, responder, cx: ConnectionTo<_>| {
                    let id = SessionId::new(format!(
                        "keke-{}",
                        next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    ));
                    let (conversation, updates) = factory
                        .open(request.cwd)
                        .await
                        .map_err(agent_client_protocol::Error::into_internal_error)?;

                    let (outcome_tx, outcome_rx) = tokio::sync::mpsc::unbounded_channel();
                    sessions.insert(
                        &id,
                        Arc::new(Entry {
                            conversation: Arc::clone(&conversation),
                            outcomes: tokio::sync::Mutex::new(outcome_rx),
                        }),
                    );
                    // Spawned, so the dispatch loop is free to deliver the
                    // permission responses the pump is about to wait on.
                    cx.spawn(pump(
                        id.clone(),
                        conversation,
                        updates,
                        outcome_tx,
                        cx.clone(),
                    ))?;
                    responder.respond(NewSessionResponse::new(id))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                async move |request: PromptRequest, responder, cx: ConnectionTo<_>| {
                    let Some(entry) = sessions.get(&request.session_id) else {
                        return responder.respond_with_error(unknown_session(&request.session_id));
                    };
                    let text = prompt_text(&request.prompt);
                    // A turn runs for as long as the model does; holding the
                    // dispatch loop for it would stop `session/cancel` from
                    // ever arriving, which is to say it would remove the only
                    // way out.
                    cx.spawn(async move {
                        let outcome = match entry.conversation.prompt(text).await {
                            Ok(()) => entry
                                .outcomes
                                .lock()
                                .await
                                .recv()
                                .await
                                .unwrap_or(AcpStopReason::EndTurn),
                            Err(error) => {
                                return responder.respond_with_internal_error(error.to_string());
                            }
                        };
                        responder.respond(PromptResponse::new(outcome))
                    })?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let sessions = Arc::clone(&sessions);
                async move |notification: CancelNotification, _cx| {
                    if let Some(entry) = sessions.get(&notification.session_id) {
                        entry.conversation.cancel();
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(Stdio::new())
        .await
}

fn unknown_session(id: &SessionId) -> agent_client_protocol::Error {
    agent_client_protocol::Error::internal_error().data(format!("unknown session `{}`", id.0))
}

/// Flatten a prompt's content blocks to the text the engine takes.
///
/// Non-text blocks are dropped rather than rendered as placeholders: a model
/// told about an image it cannot see answers about the placeholder.
fn prompt_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Forward one conversation's updates to the client for as long as it lives.
async fn pump(
    id: SessionId,
    conversation: Arc<dyn Conversation>,
    mut updates: UnboundedReceiver<Update>,
    outcomes: UnboundedSender<AcpStopReason>,
    cx: ConnectionTo<agent_client_protocol::Client>,
) -> Result<(), agent_client_protocol::Error> {
    while let Some(update) = updates.recv().await {
        match update {
            Update::TurnStarted => {}
            // ACP has no place for token accounting today, and inventing a
            // message for it would be keke's dialect rather than the protocol.
            Update::TokensUsed(_) => {}
            Update::TextDelta(text) => {
                notify(&cx, &id, SessionUpdate::AgentMessageChunk(chunk(text)))?;
            }
            Update::ThinkingDelta(text) => {
                notify(&cx, &id, SessionUpdate::AgentThoughtChunk(chunk(text)))?;
            }
            Update::ToolCallStarted(call) => {
                let started = agent_client_protocol::schema::v1::ToolCall::new(
                    call.id.to_string(),
                    call.name.clone(),
                )
                .status(ToolCallStatus::InProgress);
                notify(&cx, &id, SessionUpdate::ToolCall(started))?;
            }
            Update::ToolCallEnded(result) => {
                let fields = ToolCallUpdateFields::new()
                    .status(acp_status(result.status))
                    .content(
                        result
                            .content
                            .iter()
                            .filter_map(|block| match block {
                                keke_protocol::ContentBlock::Text { text } => {
                                    Some(text.clone().into())
                                }
                                _ => None,
                            })
                            .collect::<Vec<_>>(),
                    );
                notify(
                    &cx,
                    &id,
                    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                        result.id.to_string(),
                        fields,
                    )),
                )?;
            }
            Update::PermissionRequested {
                id: permission,
                call,
                reason,
            } => {
                // Asked and answered on this task rather than the dispatch
                // loop, which is what makes waiting for the reply safe.
                let request = RequestPermissionRequest::new(
                    id.clone(),
                    ToolCallUpdate::new(
                        call.id.to_string(),
                        ToolCallUpdateFields::new()
                            .title(format!("{}: {reason}", call.name))
                            .raw_input(call.arguments.clone()),
                    ),
                    permission_options(),
                );
                let answer = match cx.send_request(request).block_task().await {
                    Ok(response) => chosen(&response.outcome),
                    // The client went away or refused; denying is the only safe
                    // reading of no answer.
                    Err(_) => PermissionAnswer::Deny,
                };
                conversation.respond_to_permission(&permission, answer);
            }
            Update::TurnEnded(reason) => {
                let _ = outcomes.send(acp_stop_reason(&reason));
            }
            Update::Failed(message) => {
                notify(
                    &cx,
                    &id,
                    SessionUpdate::AgentMessageChunk(chunk(format!("error: {message}"))),
                )?;
                let _ = outcomes.send(AcpStopReason::Refusal);
            }
        }
    }
    Ok(())
}

fn notify(
    cx: &ConnectionTo<agent_client_protocol::Client>,
    id: &SessionId,
    update: SessionUpdate,
) -> Result<(), agent_client_protocol::Error> {
    cx.send_notification(SessionNotification::new(id.clone(), update))
}

fn chunk(text: impl Into<String>) -> ContentChunk {
    ContentChunk::new(ContentBlock::Text(TextContent::new(text.into())))
}

/// An outcome the client did not select is a refusal, not a permission.
fn chosen(outcome: &RequestPermissionOutcome) -> PermissionAnswer {
    match outcome {
        RequestPermissionOutcome::Selected(selected) => match selected.option_id.0.as_ref() {
            ALLOW => PermissionAnswer::Allow,
            ALLOW_ALWAYS => PermissionAnswer::AllowAlways,
            _ => PermissionAnswer::Deny,
        },
        _ => PermissionAnswer::Deny,
    }
}

fn acp_status(status: ToolStatus) -> ToolCallStatus {
    match status {
        ToolStatus::Ok => ToolCallStatus::Completed,
        ToolStatus::Error | ToolStatus::Denied | ToolStatus::Cancelled => ToolCallStatus::Failed,
    }
}

fn acp_stop_reason(reason: &StopReason) -> AcpStopReason {
    match reason {
        StopReason::EndTurn | StopReason::ToolUse => AcpStopReason::EndTurn,
        StopReason::MaxTokens => AcpStopReason::MaxTokens,
        StopReason::Cancelled => AcpStopReason::Cancelled,
        StopReason::Refusal { .. } => AcpStopReason::Refusal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_text_reaches_the_model() {
        let blocks = vec![
            ContentBlock::Text(TextContent::new("first")),
            ContentBlock::Text(TextContent::new("second")),
        ];
        assert_eq!(prompt_text(&blocks), "first\nsecond");
    }

    /// The option ids are a wire contract; an unrecognised one must not be read
    /// as consent.
    #[test]
    fn an_unrecognised_option_is_a_denial() {
        let selected = |id: &str| {
            let id: std::sync::Arc<str> = id.into();
            RequestPermissionOutcome::Selected(
                agent_client_protocol::schema::v1::SelectedPermissionOutcome::new(id),
            )
        };
        assert_eq!(chosen(&selected(ALLOW)), PermissionAnswer::Allow);
        assert_eq!(
            chosen(&selected(ALLOW_ALWAYS)),
            PermissionAnswer::AllowAlways
        );
        assert_eq!(chosen(&selected("something-else")), PermissionAnswer::Deny);
        assert_eq!(
            chosen(&RequestPermissionOutcome::Cancelled),
            PermissionAnswer::Deny
        );
    }

    #[test]
    fn every_offered_option_maps_back_to_an_answer() {
        for option in permission_options() {
            let outcome = RequestPermissionOutcome::Selected(
                agent_client_protocol::schema::v1::SelectedPermissionOutcome::new(
                    option.option_id.clone(),
                ),
            );
            let answer = chosen(&outcome);
            let expected = match option.kind {
                PermissionOptionKind::AllowOnce => PermissionAnswer::Allow,
                PermissionOptionKind::AllowAlways => PermissionAnswer::AllowAlways,
                _ => PermissionAnswer::Deny,
            };
            assert_eq!(answer, expected, "{:?}", option.option_id);
        }
    }
}
