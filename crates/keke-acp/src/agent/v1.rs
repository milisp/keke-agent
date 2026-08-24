//! The ACP protocol v1 surface.
//!
//! v1 is what every released client speaks today. It keeps `session/load` and
//! `session/resume` as separate methods — load restores the session *and*
//! replays the transcript, resume only restores it — and puts the turn's stop
//! reason on the `session/prompt` response. v2 folded the two methods into one
//! and moved the stop reason onto the update stream; both are served, and the
//! client chooses.

use std::sync::Arc;

use agent_client_protocol::Agent;
use agent_client_protocol::ConnectionTo;
use agent_client_protocol::schema::v1::AgentCapabilities;
use agent_client_protocol::schema::v1::CancelNotification;
use agent_client_protocol::schema::v1::ContentBlock;
use agent_client_protocol::schema::v1::ContentChunk;
use agent_client_protocol::schema::v1::Implementation;
use agent_client_protocol::schema::v1::InitializeRequest;
use agent_client_protocol::schema::v1::InitializeResponse;
use agent_client_protocol::schema::v1::ListSessionsRequest;
use agent_client_protocol::schema::v1::ListSessionsResponse;
use agent_client_protocol::schema::v1::LoadSessionRequest;
use agent_client_protocol::schema::v1::LoadSessionResponse;
use agent_client_protocol::schema::v1::NewSessionRequest;
use agent_client_protocol::schema::v1::NewSessionResponse;
use agent_client_protocol::schema::v1::PermissionOption;
use agent_client_protocol::schema::v1::PermissionOptionKind;
use agent_client_protocol::schema::v1::PromptRequest;
use agent_client_protocol::schema::v1::PromptResponse;
use agent_client_protocol::schema::v1::RequestPermissionOutcome;
use agent_client_protocol::schema::v1::RequestPermissionRequest;
use agent_client_protocol::schema::v1::ResumeSessionRequest;
use agent_client_protocol::schema::v1::ResumeSessionResponse;
use agent_client_protocol::schema::v1::SessionCapabilities;
use agent_client_protocol::schema::v1::SessionConfigOption;
use agent_client_protocol::schema::v1::SessionConfigOptionCategory;
use agent_client_protocol::schema::v1::SessionConfigSelectOption;
use agent_client_protocol::schema::v1::SessionConfigSelectOptions;
use agent_client_protocol::schema::v1::SessionId;
use agent_client_protocol::schema::v1::SessionInfo;
use agent_client_protocol::schema::v1::SessionListCapabilities;
use agent_client_protocol::schema::v1::SessionNotification;
use agent_client_protocol::schema::v1::SessionResumeCapabilities;
use agent_client_protocol::schema::v1::SessionUpdate;
use agent_client_protocol::schema::v1::SetSessionConfigOptionRequest;
use agent_client_protocol::schema::v1::SetSessionConfigOptionResponse;
use agent_client_protocol::schema::v1::StopReason as AcpStopReason;
use agent_client_protocol::schema::v1::TextContent;
use agent_client_protocol::schema::v1::ToolCallStatus;
use agent_client_protocol::schema::v1::ToolCallUpdate;
use agent_client_protocol::schema::v1::ToolCallUpdateFields;
use keke_protocol::StopReason;
use keke_protocol::ToolStatus;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;

use super::SessionFactory;
use super::Sessions;
use super::answer_for;
use super::apply;
use super::choices;
use super::enrol;
use crate::Conversation;
use crate::Opened;
use crate::PermissionAnswer;
use crate::SessionListing;
use crate::Update;

fn permission_options() -> Vec<PermissionOption> {
    vec![
        PermissionOption::new(super::ALLOW, "Allow once", PermissionOptionKind::AllowOnce),
        PermissionOption::new(
            super::ALLOW_ALWAYS,
            "Allow for the rest of this session",
            PermissionOptionKind::AllowAlways,
        ),
        PermissionOption::new(super::DENY, "Deny", PermissionOptionKind::RejectOnce),
    ]
}

/// The v1 implementation, for the router to hand a v1 client to.
pub(super) fn agent(
    factory: Arc<dyn SessionFactory>,
) -> impl agent_client_protocol::ConnectTo<agent_client_protocol::Client> {
    let sessions = Arc::new(Sessions::default());

    Agent
        .builder()
        .name("keke")
        .on_receive_request(
            async move |request: InitializeRequest, responder, _cx| {
                responder.respond(
                    InitializeResponse::new(request.protocol_version)
                        .agent_capabilities(
                            AgentCapabilities::new()
                                // `loadSession` is v1's way of saying the
                                // transcript can be replayed; v2 says it with
                                // a `replayFrom` cursor instead.
                                .load_session(true)
                                .session_capabilities(
                                    SessionCapabilities::new()
                                        .list(SessionListCapabilities::new())
                                        .resume(SessionResumeCapabilities::new()),
                                ),
                        )
                        .agent_info(Implementation::new("keke", env!("CARGO_PKG_VERSION"))),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                let factory = Arc::clone(&factory);
                async move |request: NewSessionRequest, responder, cx: ConnectionTo<_>| {
                    let opened = factory
                        .open(request.cwd)
                        .await
                        .map_err(agent_client_protocol::Error::into_internal_error)?;
                    let (id, options) = start(&sessions, opened, &cx)?;
                    responder.respond(NewSessionResponse::new(id).config_options(options))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                async move |request: PromptRequest, responder, cx: ConnectionTo<_>| {
                    let Some(entry) = sessions.get(request.session_id.0.as_ref()) else {
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
                                .map_or(AcpStopReason::EndTurn, |reason| acp_stop_reason(&reason)),
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
        .on_receive_request(
            {
                let factory = Arc::clone(&factory);
                async move |request: ListSessionsRequest, responder, _cx| {
                    let listed = factory
                        .list(request.cwd)
                        .await
                        .map_err(agent_client_protocol::Error::into_internal_error)?;
                    // Every session at once: keke lists a directory of logs, so
                    // there is no page to fetch a second one from, and a cursor
                    // promising otherwise would be a lie the client would act on.
                    responder.respond(ListSessionsResponse::new(
                        listed.into_iter().map(listed_session).collect(),
                    ))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                let factory = Arc::clone(&factory);
                async move |request: LoadSessionRequest, responder, cx: ConnectionTo<_>| {
                    let opened = match factory
                        .resume(request.session_id.to_string(), request.cwd)
                        .await
                    {
                        Ok(opened) => opened,
                        Err(error) => {
                            return responder.respond_with_internal_error(error.to_string());
                        }
                    };
                    let history = opened.history.clone();
                    let (id, options) = start(&sessions, opened, &cx)?;
                    // Replaying is what makes this `session/load` rather than
                    // `session/resume`: in v1 the transcript is the difference
                    // between the two methods.
                    replay(&cx, &id, &history)?;
                    responder.respond(LoadSessionResponse::new().config_options(options))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                let factory = Arc::clone(&factory);
                async move |request: ResumeSessionRequest, responder, cx: ConnectionTo<_>| {
                    let opened = match factory
                        .resume(request.session_id.to_string(), request.cwd)
                        .await
                    {
                        Ok(opened) => opened,
                        Err(error) => {
                            return responder.respond_with_internal_error(error.to_string());
                        }
                    };
                    // Deliberately no replay: a client that wants the
                    // transcript back asks for `session/load`.
                    let (_, options) = start(&sessions, opened, &cx)?;
                    responder.respond(ResumeSessionResponse::new().config_options(options))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                async move |request: SetSessionConfigOptionRequest, responder, _cx| {
                    let Some(entry) = sessions.get(request.session_id.0.as_ref()) else {
                        return responder.respond_with_error(unknown_session(&request.session_id));
                    };
                    let chosen = request.value.as_value_id().map(ToString::to_string);
                    match apply(&entry, request.config_id.0.as_ref(), chosen) {
                        Ok(choices) => responder
                            .respond(SetSessionConfigOptionResponse::new(rendered(&choices))),
                        Err(refusal) => responder.respond_with_error(
                            agent_client_protocol::Error::invalid_params().data(refusal),
                        ),
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let sessions = Arc::clone(&sessions);
                async move |notification: CancelNotification, _cx| {
                    if let Some(entry) = sessions.get(notification.session_id.0.as_ref()) {
                        entry.conversation.cancel();
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
}

/// Register an opened conversation and start pumping its updates.
fn start(
    sessions: &Sessions,
    opened: Opened,
    cx: &ConnectionTo<agent_client_protocol::Client>,
) -> Result<(SessionId, Vec<SessionConfigOption>), agent_client_protocol::Error> {
    // The id is the one the session is logged under, not one invented here:
    // what a client resumes must be what `session/list` showed it.
    let id = SessionId::new(opened.id.clone());
    let (outcome_tx, outcome_rx) = tokio::sync::mpsc::unbounded_channel();
    let entry = enrol(sessions, &opened, outcome_rx);
    let options = rendered(&choices(&entry));
    // Spawned, so the dispatch loop is free to deliver the permission
    // responses the pump is about to wait on.
    cx.spawn(pump(
        id.clone(),
        opened.conversation,
        opened.updates,
        outcome_tx,
        cx.clone(),
    ))?;
    Ok((id, options))
}

/// Render keke's config options in this protocol version's types.
///
/// What is offered is decided in `super::choices`; only the spelling is here,
/// because v1 and v2 declare separate types with the same names.
fn rendered(choices: &[super::Choice]) -> Vec<SessionConfigOption> {
    choices
        .iter()
        .map(|choice| {
            SessionConfigOption::select(
                choice.id,
                choice.name,
                choice.current.clone(),
                SessionConfigSelectOptions::Ungrouped(
                    choice
                        .options
                        .iter()
                        .map(|(value, label)| {
                            SessionConfigSelectOption::new(value.clone(), label.clone())
                        })
                        .collect(),
                ),
            )
            // The category is what tells a client this is the model picker
            // rather than one more setting to bury in a menu.
            .category(SessionConfigOptionCategory::Model)
        })
        .collect()
}

/// Send a loaded session's history to the client as ordinary updates.
fn replay(
    cx: &ConnectionTo<agent_client_protocol::Client>,
    id: &SessionId,
    history: &[keke_protocol::Message],
) -> Result<(), agent_client_protocol::Error> {
    for message in history {
        if let Some(update) = replayed(message) {
            notify(cx, id, update)?;
        }
    }
    Ok(())
}

/// How one logged message reads on the wire, or `None` if it does not.
///
/// v1 has only chunks, so a replayed message is one chunk of its own.
fn replayed(message: &keke_protocol::Message) -> Option<SessionUpdate> {
    let text = message.text();
    if text.is_empty() {
        return None;
    }
    match message.role {
        keke_protocol::Role::User => Some(SessionUpdate::UserMessageChunk(chunk(text))),
        keke_protocol::Role::Assistant => Some(SessionUpdate::AgentMessageChunk(chunk(text))),
        // System prompts and tool results are the engine talking to itself.
        keke_protocol::Role::System | keke_protocol::Role::Tool => None,
    }
}

/// Describe one previous session for `session/list`.
fn listed_session(listing: SessionListing) -> SessionInfo {
    SessionInfo::new(listing.id, listing.cwd)
        .title(listing.title)
        .updated_at(listing.updated_at)
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
    outcomes: UnboundedSender<StopReason>,
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
                let _ = outcomes.send(reason);
            }
            Update::Failed(message) => {
                notify(
                    &cx,
                    &id,
                    SessionUpdate::AgentMessageChunk(chunk(format!("error: {message}"))),
                )?;
                let _ = outcomes.send(StopReason::Refusal { message });
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
        RequestPermissionOutcome::Selected(selected) => answer_for(selected.option_id.0.as_ref()),
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

    /// A cancelled permission request is not a selection, so it must not be
    /// read as one.
    #[test]
    fn a_cancelled_request_is_a_denial() {
        assert_eq!(
            chosen(&RequestPermissionOutcome::Cancelled),
            PermissionAnswer::Deny
        );
    }
}
