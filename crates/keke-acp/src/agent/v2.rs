//! The ACP protocol v2 surface.
//!
//! v2 folded v1's `session/load` into `session/resume` — one method that
//! restores the session and replays the transcript only when the client names a
//! `replayFrom` cursor — moved the turn's stop reason off the `session/prompt`
//! response onto the `session/update` state stream, flattened tool-call
//! creation and update into one message, and hung streamed content off a
//! message id.

use std::sync::Arc;

use agent_client_protocol::Agent;
use agent_client_protocol::ConnectionTo;
use agent_client_protocol::schema::v2::AbsolutePath;
use agent_client_protocol::schema::v2::AgentCapabilities;
use agent_client_protocol::schema::v2::AgentMessage;
use agent_client_protocol::schema::v2::AuthMethod;
use agent_client_protocol::schema::v2::AuthMethodAgent;
use agent_client_protocol::schema::v2::AvailableCommand;
use agent_client_protocol::schema::v2::AvailableCommandsUpdate;
use agent_client_protocol::schema::v2::CancelSessionNotification;
use agent_client_protocol::schema::v2::ContentBlock;
use agent_client_protocol::schema::v2::ContentChunk;
use agent_client_protocol::schema::v2::IdleStateUpdate;
use agent_client_protocol::schema::v2::Implementation;
use agent_client_protocol::schema::v2::InitializeRequest;
use agent_client_protocol::schema::v2::InitializeResponse;
use agent_client_protocol::schema::v2::ListSessionsRequest;
use agent_client_protocol::schema::v2::ListSessionsResponse;
use agent_client_protocol::schema::v2::LoginAuthRequest;
use agent_client_protocol::schema::v2::LoginAuthResponse;
use agent_client_protocol::schema::v2::MessageId;
use agent_client_protocol::schema::v2::NewSessionRequest;
use agent_client_protocol::schema::v2::NewSessionResponse;
use agent_client_protocol::schema::v2::PermissionOption;
use agent_client_protocol::schema::v2::PermissionOptionKind;
use agent_client_protocol::schema::v2::PromptRequest;
use agent_client_protocol::schema::v2::PromptResponse;
use agent_client_protocol::schema::v2::RequestPermissionOutcome;
use agent_client_protocol::schema::v2::RequestPermissionRequest;
use agent_client_protocol::schema::v2::RequestPermissionSubject;
use agent_client_protocol::schema::v2::ResumeSessionRequest;
use agent_client_protocol::schema::v2::ResumeSessionResponse;
use agent_client_protocol::schema::v2::RunningStateUpdate;
use agent_client_protocol::schema::v2::SessionCapabilities;
use agent_client_protocol::schema::v2::SessionConfigOption;
use agent_client_protocol::schema::v2::SessionConfigOptionCategory;
use agent_client_protocol::schema::v2::SessionConfigSelectOption;
use agent_client_protocol::schema::v2::SessionConfigSelectOptions;
use agent_client_protocol::schema::v2::SessionId;
use agent_client_protocol::schema::v2::SessionInfo;
use agent_client_protocol::schema::v2::SessionUpdate;
use agent_client_protocol::schema::v2::SetSessionConfigOptionRequest;
use agent_client_protocol::schema::v2::SetSessionConfigOptionResponse;
use agent_client_protocol::schema::v2::StateUpdate;
use agent_client_protocol::schema::v2::StopReason as AcpStopReason;
use agent_client_protocol::schema::v2::TextContent;
use agent_client_protocol::schema::v2::ToolCallContent;
use agent_client_protocol::schema::v2::ToolCallStatus;
use agent_client_protocol::schema::v2::ToolCallUpdate;
use agent_client_protocol::schema::v2::UpdateSessionNotification;
use agent_client_protocol::schema::v2::UserMessage;
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

/// Render the factory's auth methods in this wire's terms.
///
/// Credential state rides in `_meta`: ACP has no field for "already signed
/// in", but a client that shows a sign-in menu needs it to avoid offering a
/// login for a route that has one.
fn auth_methods(factory: &dyn SessionFactory) -> Vec<AuthMethod> {
    factory
        .auth_methods()
        .into_iter()
        .map(|method| {
            let mut meta = serde_json::Map::new();
            meta.insert("signedIn".to_string(), method.signed_in.into());
            if let Some(source) = method.source {
                meta.insert("credentialSource".to_string(), source.into());
            }
            AuthMethod::Agent(
                AuthMethodAgent::new(method.id, method.name)
                    .description(method.description)
                    .meta(meta),
            )
        })
        .collect()
}

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

/// The v2 implementation, for the router to hand a v2 client to.
///
/// `Agent.v2()` rather than `builder()`: the latter declares a *v1* endpoint
/// whatever types the handlers are written against, which is a mistake nothing
/// but the wire can catch.
pub(super) fn agent(
    factory: Arc<dyn SessionFactory>,
) -> impl agent_client_protocol::ConnectTo<agent_client_protocol::Client> {
    let sessions = Arc::new(Sessions::default());

    Agent
        .v2()
        .name("keke")
        .on_receive_request(
            {
                let factory = Arc::clone(&factory);
                async move |request: InitializeRequest, responder, _cx| {
                    responder.respond(
                        InitializeResponse::new(
                            request.protocol_version,
                            Implementation::new("keke", env!("CARGO_PKG_VERSION")),
                        )
                        // `session` present at all is what says keke speaks the
                        // session surface; `session/list` and `session/resume`
                        // are part of it rather than capabilities of their own.
                        .capabilities(AgentCapabilities::new().session(SessionCapabilities::new()))
                        .auth_methods(auth_methods(factory.as_ref())),
                    )
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let factory = Arc::clone(&factory);
                async move |request: LoginAuthRequest, responder, _cx| {
                    factory
                        .authenticate(request.method_id.0.as_ref(), request.meta)
                        .await
                        .map_err(agent_client_protocol::Error::into_internal_error)?;
                    responder.respond(LoginAuthResponse::new())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                let factory = Arc::clone(&factory);
                async move |request: NewSessionRequest, responder, cx: ConnectionTo<_>| {
                    let opened = factory
                        .open(request.cwd.into_inner())
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
                        match entry.conversation.prompt(text).await {
                            // The turn's outcome travels as the idle state
                            // update the pump sends; waiting for it here is
                            // what keeps the response from arriving before the
                            // work it stands for.
                            Ok(()) => {
                                let _ = entry.outcomes.lock().await.recv().await;
                            }
                            Err(error) => {
                                return responder.respond_with_internal_error(error.to_string());
                            }
                        }
                        responder.respond(PromptResponse::new())
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
                        .list(request.cwd.map(AbsolutePath::into_inner))
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
                async move |request: ResumeSessionRequest, responder, cx: ConnectionTo<_>| {
                    let opened = match factory
                        .resume(request.session_id.to_string(), request.cwd.into_inner())
                        .await
                    {
                        Ok(opened) => opened,
                        Err(error) => {
                            return responder.respond_with_internal_error(error.to_string());
                        }
                    };
                    let history = opened.history.clone();
                    let (id, options) = start(&sessions, opened, &cx)?;
                    // Resuming restores the session; replaying the transcript
                    // is the separate thing v1 spelled `session/load`, and in
                    // v2 a client asks for it by naming where to replay from.
                    // A surface that already holds the transcript would
                    // otherwise draw every message twice.
                    if request.replay_from.is_some() {
                        replay(&cx, &id, &history)?;
                    }
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
                    let chosen = request.value.as_id().map(ToString::to_string);
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
                async move |notification: CancelSessionNotification, _cx| {
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
///
/// Shared by `session/new` and `session/resume` so the two cannot drift: a
/// resumed session that skipped the pump would accept prompts and report
/// nothing.
fn start(
    sessions: &Sessions,
    opened: Opened,
    cx: &ConnectionTo<agent_client_protocol::Client>,
) -> Result<(SessionId, Vec<SessionConfigOption>), agent_client_protocol::Error> {
    // The id is the one the session is logged under, not one invented here:
    // what a client resumes must be what `session/list` showed it.
    let id = SessionId::new(opened.id.clone());
    let commands = opened.commands.clone();
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
    // Sent even when empty is skipped: a client that never hears about
    // commands assumes it has none, same as one keke never told.
    if !commands.is_empty() {
        notify(cx, &id, available_commands_update(&commands))?;
    }
    Ok((id, options))
}

/// What a plugin contributes, in the client's own autocomplete.
fn available_commands_update(commands: &[crate::PluginCommand]) -> SessionUpdate {
    SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(
        commands
            .iter()
            .map(|command| AvailableCommand::new(command.name.clone(), command.description.clone()))
            .collect(),
    ))
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
                choice.current.as_str(),
                SessionConfigSelectOptions::Ungrouped(
                    choice
                        .options
                        .iter()
                        .map(|(value, label)| {
                            SessionConfigSelectOption::new(value.as_str(), label.as_str())
                        })
                        .collect(),
                ),
            )
            // The category is what tells a client which menu a choice
            // belongs in, rather than lumping the effort ladder in with the
            // model picker.
            .category(category_for(choice.id))
        })
        .collect()
}

fn category_for(id: &str) -> SessionConfigOptionCategory {
    if id == super::REASONING_EFFORT {
        SessionConfigOptionCategory::ThoughtLevel
    } else if id == super::APPROVAL_POLICY {
        SessionConfigOptionCategory::Mode
    } else {
        SessionConfigOptionCategory::Model
    }
}

/// Send a resumed session's history to the client as ordinary updates.
///
/// Only what a person could see is replayed: the tool traffic between two
/// messages is in the log, but a transcript that reruns it would look like work
/// happening now.
fn replay(
    cx: &ConnectionTo<agent_client_protocol::Client>,
    id: &SessionId,
    history: &[keke_protocol::Message],
) -> Result<(), agent_client_protocol::Error> {
    for (at, message) in history.iter().enumerate() {
        if let Some(update) = replayed(at, message) {
            notify(cx, id, update)?;
        }
    }
    Ok(())
}

/// How one logged message reads on the wire, or `None` if it does not.
///
/// Replayed as whole messages rather than chunks: nothing is streaming, and a
/// chunk says otherwise to a client that renders the two differently.
fn replayed(at: usize, message: &keke_protocol::Message) -> Option<SessionUpdate> {
    let text = message.text();
    if text.is_empty() {
        return None;
    }
    let content = vec![ContentBlock::Text(TextContent::new(text))];
    let id = MessageId::new(format!("replay-{at}"));
    match message.role {
        keke_protocol::Role::User => Some(SessionUpdate::UserMessage(
            UserMessage::new(id).content(content),
        )),
        keke_protocol::Role::Assistant => Some(SessionUpdate::AgentMessage(
            AgentMessage::new(id).content(content),
        )),
        // System prompts and tool results are the engine talking to itself.
        keke_protocol::Role::System | keke_protocol::Role::Tool => None,
    }
}

/// Describe one previous session for `session/list`.
fn listed_session(listing: SessionListing) -> SessionInfo {
    SessionInfo::new(listing.id, AbsolutePath::new(listing.cwd))
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
    // v2 hangs streamed content off a message id, so the chunks of one turn's
    // answer can be recognised as one message. A counter is enough: the ids
    // only have to be distinct within the session.
    let mut turn = 0_u64;
    while let Some(update) = updates.recv().await {
        match update {
            Update::TurnStarted => {
                turn += 1;
                notify(
                    &cx,
                    &id,
                    SessionUpdate::StateUpdate(StateUpdate::Running(RunningStateUpdate::new())),
                )?;
            }
            // ACP has no place for token accounting today, and inventing a
            // message for it would be keke's dialect rather than the protocol.
            Update::TokensUsed(_) => {}
            Update::TextDelta(text) => {
                notify(
                    &cx,
                    &id,
                    SessionUpdate::AgentMessageChunk(chunk(text, message_id(turn, "agent"))),
                )?;
            }
            Update::ThinkingDelta(text) => {
                notify(
                    &cx,
                    &id,
                    SessionUpdate::AgentThoughtChunk(chunk(text, message_id(turn, "thought"))),
                )?;
            }
            Update::ToolCallStarted(call) => {
                notify(
                    &cx,
                    &id,
                    SessionUpdate::ToolCallUpdate(
                        ToolCallUpdate::new(call.id.to_string())
                            .title(call.name.clone())
                            .status(ToolCallStatus::InProgress)
                            .raw_input(call.arguments.clone()),
                    ),
                )?;
            }
            Update::ToolCallEnded(result) => {
                let content: Vec<ToolCallContent> = result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        keke_protocol::ContentBlock::Text { text } => {
                            Some(ToolCallContent::from(text.clone()))
                        }
                        _ => None,
                    })
                    .collect();
                notify(
                    &cx,
                    &id,
                    SessionUpdate::ToolCallUpdate(
                        ToolCallUpdate::new(result.id.to_string())
                            .status(acp_status(result.status))
                            .content(content),
                    ),
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
                    format!("{}: {reason}", call.name),
                    permission_options(),
                )
                .subject(RequestPermissionSubject::from(
                    ToolCallUpdate::new(call.id.to_string())
                        .title(call.name.clone())
                        .raw_input(call.arguments.clone()),
                ));
                let answer = match cx.send_request(request).block_task().await {
                    Ok(response) => chosen(&response.outcome),
                    // The client went away or refused; denying is the only safe
                    // reading of no answer.
                    Err(_) => PermissionAnswer::Deny,
                };
                conversation.respond_to_permission(&permission, answer);
            }
            Update::TurnEnded(reason) => {
                idle(&cx, &id, acp_stop_reason(&reason))?;
                let _ = outcomes.send(reason);
            }
            Update::Failed(message) => {
                notify(
                    &cx,
                    &id,
                    SessionUpdate::AgentMessageChunk(chunk(
                        format!("error: {message}"),
                        message_id(turn, "agent"),
                    )),
                )?;
                idle(&cx, &id, AcpStopReason::Refusal)?;
                let _ = outcomes.send(StopReason::Refusal {
                    message: message.clone(),
                });
            }
            // A remote client starts over with `session/new`, which already
            // gives it a session with nothing in it; this is only a signal an
            // in-process surface uses to reset what it has drawn.
            Update::SessionReset => {}
        }
    }
    Ok(())
}

/// Report the turn as finished, and why.
///
/// v2 dropped the stop reason from the `session/prompt` response, so this is
/// the only place a client learns whether the turn ended or was stopped.
fn idle(
    cx: &ConnectionTo<agent_client_protocol::Client>,
    id: &SessionId,
    reason: AcpStopReason,
) -> Result<(), agent_client_protocol::Error> {
    notify(
        cx,
        id,
        SessionUpdate::StateUpdate(StateUpdate::Idle(
            IdleStateUpdate::new().stop_reason(reason),
        )),
    )
}

/// The id the chunks of one message are streamed under.
fn message_id(turn: u64, kind: &str) -> MessageId {
    MessageId::new(format!("{turn}-{kind}"))
}

fn notify(
    cx: &ConnectionTo<agent_client_protocol::Client>,
    id: &SessionId,
    update: SessionUpdate,
) -> Result<(), agent_client_protocol::Error> {
    cx.send_notification(UpdateSessionNotification::new(id.clone(), update))
}

fn chunk(text: impl Into<String>, message: MessageId) -> ContentChunk {
    ContentChunk::new(ContentBlock::Text(TextContent::new(text.into())), message)
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
    use agent_client_protocol::schema::v2::SelectedPermissionOutcome;

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
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id))
        };
        assert_eq!(
            chosen(&selected(super::super::ALLOW)),
            PermissionAnswer::Allow
        );
        assert_eq!(
            chosen(&selected(super::super::ALLOW_ALWAYS)),
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
            let outcome = RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                option.option_id.clone(),
            ));
            let answer = chosen(&outcome);
            let expected = match option.kind {
                PermissionOptionKind::AllowOnce => PermissionAnswer::Allow,
                PermissionOptionKind::AllowAlways => PermissionAnswer::AllowAlways,
                _ => PermissionAnswer::Deny,
            };
            assert_eq!(answer, expected, "{:?}", option.option_id);
        }
    }

    /// A replayed transcript is what a person said and what the agent said
    /// back. The system prompt and the tool traffic are in the log too, and
    /// showing them would be keke narrating its own plumbing.
    #[test]
    fn only_what_a_person_could_see_is_replayed() {
        let history = [
            keke_protocol::Message {
                role: keke_protocol::Role::System,
                content: vec![keke_protocol::ContentBlock::text("you are keke")],
            },
            keke_protocol::Message::user("hello"),
            keke_protocol::Message::assistant("hi"),
            keke_protocol::Message {
                role: keke_protocol::Role::Tool,
                content: vec![keke_protocol::ContentBlock::text("{}")],
            },
        ];

        let replayed: Vec<_> = history
            .iter()
            .enumerate()
            .filter_map(|(at, message)| super::replayed(at, message))
            .collect();

        assert_eq!(replayed.len(), 2, "{replayed:?}");
        assert!(matches!(replayed[0], SessionUpdate::UserMessage(_)));
        assert!(matches!(replayed[1], SessionUpdate::AgentMessage(_)));
    }

    /// The id a client is shown is the id it can resume, so a listing carries
    /// the session's own id rather than a position in the list.
    #[test]
    fn a_listing_carries_the_id_a_client_resumes_with() {
        let info = listed_session(SessionListing {
            id: "01930000-0000-7000-8000-000000000000".to_string(),
            cwd: std::path::PathBuf::from("/work"),
            title: "fix the parser".to_string(),
            updated_at: "2026-08-23T10:00:00Z".to_string(),
        });

        assert_eq!(
            info.session_id.0.as_ref(),
            "01930000-0000-7000-8000-000000000000"
        );
        assert_eq!(info.cwd.into_inner(), std::path::PathBuf::from("/work"));
        assert_eq!(info.title.as_deref(), Some("fix the parser"));
    }
}
