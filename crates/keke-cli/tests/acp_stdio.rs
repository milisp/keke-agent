//! Drives the real `keke agent stdio` binary with a real ACP client.
//!
//! The unit tests prove the pieces translate correctly. This one proves an
//! editor can actually talk to keke: a prompt goes in over stdin, notifications
//! come back over stdout, and a permission request reaches the client and its
//! answer reaches the tool.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::Mutex;

use agent_client_protocol::AcpAgent;
use agent_client_protocol::AcpAgentConfig;
use agent_client_protocol::Agent;
use agent_client_protocol::ConnectionTo;
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v2::AbsolutePath;
use agent_client_protocol::schema::v2::ContentBlock;
use agent_client_protocol::schema::v2::Implementation;
use agent_client_protocol::schema::v2::InitializeRequest;
use agent_client_protocol::schema::v2::ListSessionsRequest;
use agent_client_protocol::schema::v2::NewSessionRequest;
use agent_client_protocol::schema::v2::PromptRequest;
use agent_client_protocol::schema::v2::ReplayFrom;
use agent_client_protocol::schema::v2::ReplayFromStart;
use agent_client_protocol::schema::v2::RequestPermissionOutcome;
use agent_client_protocol::schema::v2::RequestPermissionRequest;
use agent_client_protocol::schema::v2::RequestPermissionResponse;
use agent_client_protocol::schema::v2::ResumeSessionRequest;
use agent_client_protocol::schema::v2::SelectedPermissionOutcome;
use agent_client_protocol::schema::v2::SessionConfigKind;
use agent_client_protocol::schema::v2::SessionConfigOptionCategory;
use agent_client_protocol::schema::v2::SessionUpdate;
use agent_client_protocol::schema::v2::SetSessionConfigOptionRequest;
use agent_client_protocol::schema::v2::StateUpdate;
use agent_client_protocol::schema::v2::StopReason;
use agent_client_protocol::schema::v2::TextContent;
use agent_client_protocol::schema::v2::ToolCallStatus;
use agent_client_protocol::schema::v2::UpdateSessionNotification;
use keke_test_support::Endpoint;
use keke_test_support::MockInferenceServer;
use keke_test_support::Reply;

/// What the client saw, in arrival order.
#[derive(Default)]
struct Seen {
    text: String,
    tool_calls: Vec<String>,
    tool_statuses: Vec<ToolCallStatus>,
    stop_reasons: Vec<StopReason>,
    permissions: Vec<String>,
    /// Role and text of each message a resumed session replayed.
    replayed: Vec<(String, String)>,
}

#[tokio::test(flavor = "multi_thread")]
async fn an_editor_prompts_keke_and_answers_its_permission_request() {
    let home = tempfile::tempdir().expect("tempdir");
    let workspace = tempfile::tempdir().expect("tempdir");
    let server = MockInferenceServer::start().await;

    // A command needs approval, so the editor has to be asked before it runs.
    server.script(
        Endpoint::ChatCompletions,
        Reply::tool_call("bash", serde_json::json!({ "command": "echo hello" })),
    );
    server.script(Endpoint::ChatCompletions, Reply::text("it printed hello"));

    let agent = AcpAgent::new(
        AcpAgentConfig::new(env!("CARGO_BIN_EXE_keke"))
            .args(["agent", "stdio"])
            .env("KEKE_HOME", home.path().display().to_string())
            // Shared machine state: the OS keyring and another tool's login
            // would both make this pass or fail depending on the developer.
            .env("KEKE_CREDENTIAL_STORE", "file")
            .env("KEKE_IMPORT", "off")
            // Pinned rather than removed, so an ambient override in the
            // developer's shell cannot redirect the run.
            .env("KEKE_PROVIDER", "grok")
            .env("KEKE_MODEL", "grok-4.6")
            .env("XAI_BASE_URL", server.base_url())
            .env("XAI_API_KEY", "test-key"),
    );

    let seen = Arc::new(Mutex::new(Seen::default()));

    agent_client_protocol::Client
        // v2, matching the endpoint: a v1 client's own compat layer would
        // rewrite the version it asks for, and the test would then pass
        // against whichever version keke happened to serve.
        .v2()
        .on_receive_notification(
            {
                let seen = Arc::clone(&seen);
                async move |notification: UpdateSessionNotification, _cx| {
                    let mut seen = seen.lock().expect("lock");
                    match notification.update {
                        SessionUpdate::AgentMessageChunk(chunk) => {
                            if let ContentBlock::Text(text) = chunk.content {
                                seen.text.push_str(&text.text);
                            }
                        }
                        // v2 folded creation and update into one message, so
                        // a title names a call that has just started and a
                        // status reports where it got to.
                        SessionUpdate::ToolCallUpdate(update) => {
                            if let Some(title) = update.title.take() {
                                seen.tool_calls.push(title);
                            }
                            if let Some(status) = update.status.take() {
                                seen.tool_statuses.push(status);
                            }
                        }
                        SessionUpdate::StateUpdate(StateUpdate::Idle(idle)) => {
                            seen.stop_reasons.extend(idle.stop_reason);
                        }
                        _ => {}
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let seen = Arc::clone(&seen);
                async move |request: RequestPermissionRequest, responder, _cx| {
                    seen.lock()
                        .expect("lock")
                        .permissions
                        .push(request.title.clone());
                    // Choose the option keke offered by name, not by position:
                    // the ids are the contract, and picking `first()` would
                    // pass even if the list were reordered into a denial.
                    let allow = request
                        .options
                        .iter()
                        .find(|option| option.option_id.0.as_ref() == "allow")
                        .expect("keke must offer an allow option");
                    responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                            allow.option_id.clone(),
                        )),
                    ))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, {
            let cwd = workspace.path().to_path_buf();
            |connection: ConnectionTo<Agent>| async move {
                connection
                    .send_request(InitializeRequest::new(
                        ProtocolVersion::V2,
                        Implementation::new("test-editor", "0.0.0"),
                    ))
                    .block_task()
                    .await?;

                let session = connection
                    .send_request(NewSessionRequest::new(AbsolutePath::new(cwd)))
                    .block_task()
                    .await?
                    .session_id;

                let reply = connection
                    .send_request(PromptRequest::new(
                        session,
                        vec![ContentBlock::Text(TextContent::new("say hello"))],
                    ))
                    .block_task()
                    .await?;
                // v2 moved the stop reason onto the state updates; the
                // response only says the turn is over.
                let _ = reply;
                Ok(())
            }
        })
        .await
        .expect("the ACP session runs to completion");

    let seen = seen.lock().expect("lock");
    assert!(
        seen.text.contains("it printed hello"),
        "the answer must reach the editor: {:?}",
        seen.text
    );
    assert_eq!(
        seen.tool_calls,
        vec!["bash".to_string()],
        "the editor must be told which tool ran"
    );
    assert_eq!(
        seen.tool_statuses,
        vec![ToolCallStatus::InProgress, ToolCallStatus::Completed],
        "an approved call must not come back failed"
    );
    assert_eq!(
        seen.stop_reasons,
        vec![StopReason::EndTurn],
        "the turn's outcome reaches the editor as the idle state it ends in"
    );
    assert_eq!(
        seen.permissions.len(),
        1,
        "the editor must have been asked exactly once: {:?}",
        seen.permissions
    );
}

/// A second run of the binary must be able to find the first run's session and
/// pick it up: this is the whole of what `session/list` and `session/resume`
/// are for, and neither is provable inside one process.
#[tokio::test(flavor = "multi_thread")]
async fn a_later_run_lists_and_resumes_the_conversation() {
    let home = tempfile::tempdir().expect("tempdir");
    let workspace = tempfile::tempdir().expect("tempdir");
    let server = MockInferenceServer::start().await;
    server.script(Endpoint::ChatCompletions, Reply::text("the first answer"));
    server.script(Endpoint::ChatCompletions, Reply::text("the second answer"));

    let agent = || {
        AcpAgent::new(
            AcpAgentConfig::new(env!("CARGO_BIN_EXE_keke"))
                .args(["agent", "stdio"])
                .env("KEKE_HOME", home.path().display().to_string())
                .env("KEKE_CREDENTIAL_STORE", "file")
                .env("KEKE_IMPORT", "off")
                .env("KEKE_PROVIDER", "grok")
                .env("KEKE_MODEL", "grok-4.6")
                .env("XAI_BASE_URL", server.base_url())
                .env("XAI_API_KEY", "test-key"),
        )
    };
    let cwd = workspace.path().to_path_buf();

    // The first run says something worth resuming.
    let started = agent_client_protocol::Client
        .v2()
        .connect_with(agent(), {
            let cwd = cwd.clone();
            |connection: ConnectionTo<Agent>| async move {
                initialize(&connection).await?;
                let session = connection
                    .send_request(NewSessionRequest::new(AbsolutePath::new(cwd)))
                    .block_task()
                    .await?
                    .session_id;
                connection
                    .send_request(PromptRequest::new(
                        session.clone(),
                        vec![ContentBlock::Text(TextContent::new("remember this"))],
                    ))
                    .block_task()
                    .await?;
                Ok(session)
            }
        })
        .await
        .expect("the first ACP session runs to completion");

    let seen = Arc::new(Mutex::new(Seen::default()));
    agent_client_protocol::Client
        // v2, matching the endpoint: a v1 client's own compat layer would
        // rewrite the version it asks for, and the test would then pass
        // against whichever version keke happened to serve.
        .v2()
        .on_receive_notification(
            {
                let seen = Arc::clone(&seen);
                async move |notification: UpdateSessionNotification, _cx| {
                    let mut seen = seen.lock().expect("lock");
                    match notification.update {
                        SessionUpdate::UserMessage(message) => {
                            seen.replayed
                                .push(("user".to_string(), text_of(message.content.take())));
                        }
                        SessionUpdate::AgentMessage(message) => {
                            seen.replayed
                                .push(("agent".to_string(), text_of(message.content.take())));
                        }
                        _ => {}
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(agent(), {
            let cwd = cwd.clone();
            let started = started.clone();
            |connection: ConnectionTo<Agent>| async move {
                initialize(&connection).await?;
                let listed = connection
                    .send_request(ListSessionsRequest::new())
                    .block_task()
                    .await?
                    .sessions;
                assert!(
                    listed.iter().any(|info| info.session_id == started),
                    "the session the first run wrote must be listed: {listed:?}"
                );

                connection
                    .send_request(
                        ResumeSessionRequest::new(started.clone(), AbsolutePath::new(cwd))
                            .replay_from(ReplayFrom::from(ReplayFromStart::new())),
                    )
                    .block_task()
                    .await?;
                connection
                    .send_request(PromptRequest::new(
                        started,
                        vec![ContentBlock::Text(TextContent::new("and again"))],
                    ))
                    .block_task()
                    .await?;
                Ok(())
            }
        })
        .await
        .expect("the second ACP session runs to completion");

    let seen = seen.lock().expect("lock");
    assert!(
        seen.replayed
            .contains(&("user".to_string(), "remember this".to_string())),
        "the replayed transcript must carry what the person said: {:?}",
        seen.replayed
    );
    assert!(
        seen.replayed
            .contains(&("agent".to_string(), "the first answer".to_string())),
        "and what the agent answered: {:?}",
        seen.replayed
    );
}

/// The handshake both runs need, kept in one place.
async fn initialize(connection: &ConnectionTo<Agent>) -> Result<(), agent_client_protocol::Error> {
    connection
        .send_request(InitializeRequest::new(
            ProtocolVersion::V2,
            Implementation::new("test-editor", "0.0.0"),
        ))
        .block_task()
        .await?;
    Ok(())
}

fn text_of(content: Option<Vec<ContentBlock>>) -> String {
    content
        .unwrap_or_default()
        .into_iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text),
            _ => None,
        })
        .collect()
}

/// The editor is where a person picks a model, so keke has to say what there is
/// to pick and the pick has to reach the next request. Asserted against what
/// the provider was actually asked, not against what keke reported back.
#[tokio::test(flavor = "multi_thread")]
async fn an_editor_switches_the_model_and_the_next_request_uses_it() {
    let home = tempfile::tempdir().expect("tempdir");
    let workspace = tempfile::tempdir().expect("tempdir");
    let server = MockInferenceServer::start().await;
    server.set_models(["grok-4.6", "grok-4-fast"]);
    server.script(Endpoint::ChatCompletions, Reply::text("answered"));

    let agent = AcpAgent::new(
        AcpAgentConfig::new(env!("CARGO_BIN_EXE_keke"))
            .args(["agent", "stdio"])
            .env("KEKE_HOME", home.path().display().to_string())
            .env("KEKE_CREDENTIAL_STORE", "file")
            .env("KEKE_IMPORT", "off")
            .env("KEKE_PROVIDER", "grok")
            .env("KEKE_MODEL", "grok-4.6")
            .env("XAI_BASE_URL", server.base_url())
            .env("XAI_API_KEY", "test-key"),
    );

    agent_client_protocol::Client
        // v2, matching the endpoint: a v1 client's own compat layer would
        // rewrite the version it asks for, and the test would then pass
        // against whichever version keke happened to serve.
        .v2()
        .connect_with(agent, {
            let cwd = workspace.path().to_path_buf();
            |connection: ConnectionTo<Agent>| async move {
                initialize(&connection).await?;
                let session = connection
                    .send_request(NewSessionRequest::new(AbsolutePath::new(cwd)))
                    .block_task()
                    .await?;

                let offered = session
                    .config_options
                    .iter()
                    .find(|option| option.config_id.0.as_ref() == "model")
                    .expect("keke must offer a model to choose");
                assert_eq!(
                    offered.category,
                    Some(SessionConfigOptionCategory::Model),
                    "the category is what marks this as the model picker"
                );
                let SessionConfigKind::Select(select) = &offered.kind else {
                    panic!("a model choice is a select: {:?}", offered.kind);
                };
                assert_eq!(select.current_value.to_string(), "grok-4.6");

                connection
                    .send_request(SetSessionConfigOptionRequest::new(
                        session.session_id.clone(),
                        "model",
                        "grok-4-fast",
                    ))
                    .block_task()
                    .await?;
                connection
                    .send_request(PromptRequest::new(
                        session.session_id,
                        vec![ContentBlock::Text(TextContent::new("say something"))],
                    ))
                    .block_task()
                    .await?;
                Ok(())
            }
        })
        .await
        .expect("the ACP session runs to completion");

    let asked: Vec<_> = server
        .requests_to(Endpoint::ChatCompletions)
        .iter()
        .filter_map(|request| request.model().map(str::to_string))
        .collect();
    assert_eq!(
        asked,
        vec!["grok-4-fast".to_string()],
        "the chosen model must be the one the provider was asked for"
    );
}

/// What is actually on the wire, driven with bytes rather than with the SDK's
/// client.
///
/// The SDK's client rewrites `protocolVersion` to whatever version *it* was
/// built for, so a test written against it passes whichever version keke
/// serves — which is how keke shipped a v1 endpoint with v2 handlers. A pipe
/// has nobody to do the rewriting.
///
/// Both versions, because the point of the router is that a client picks: a
/// regression that drops either one is a client that cannot connect at all.
#[tokio::test(flavor = "multi_thread")]
async fn the_endpoint_on_the_wire_answers_whichever_version_was_asked_for() {
    let v1 = initialize_over_a_pipe(
        br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
    );
    assert_eq!(v1["result"]["protocolVersion"], 1, "{v1}");
    assert_eq!(
        v1["result"]["agentCapabilities"]["sessionCapabilities"]["list"],
        serde_json::json!({}),
        "a v1 client learns about `session/list` from its own capability flag: {v1}"
    );
    assert_eq!(
        v1["result"]["agentCapabilities"]["loadSession"], true,
        "and about replay from `loadSession`: {v1}"
    );

    let v2 = initialize_over_a_pipe(
        br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":2,"info":{"name":"web-ui","version":"0.1"},"capabilities":{}}}
"#,
    );
    assert_eq!(v2["result"]["protocolVersion"], 2, "{v2}");
    assert!(
        v2["result"]["capabilities"]["session"].is_object(),
        "v2 says the same thing with one `session` object: {v2}"
    );
}

/// Send one raw request to `keke agent stdio` and read the one raw response.
fn initialize_over_a_pipe(request: &[u8]) -> serde_json::Value {
    let home = tempfile::tempdir().expect("tempdir");
    let mut keke = std::process::Command::new(env!("CARGO_BIN_EXE_keke"))
        .args(["agent", "stdio"])
        .env("KEKE_HOME", home.path().display().to_string())
        .env("KEKE_CREDENTIAL_STORE", "file")
        .env("KEKE_IMPORT", "off")
        .env("KEKE_PROVIDER", "grok")
        .env("KEKE_MODEL", "grok-4.6")
        .env("XAI_API_KEY", "test-key")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("keke agent stdio starts");

    use std::io::Write;
    keke.stdin
        .take()
        .expect("stdin")
        .write_all(request)
        .expect("the request is written");

    let output = keke.wait_with_output().expect("keke exits with the pipe");
    let line = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(line.lines().next().unwrap_or_default()).expect("one JSON response")
}

/// The v1 path, end to end with a v1 client: the stop reason comes back on the
/// `session/prompt` response, and `session/load` replays the transcript that
/// `session/resume` deliberately does not.
#[tokio::test(flavor = "multi_thread")]
async fn a_v1_client_prompts_and_loads_back_the_transcript() {
    use agent_client_protocol::schema::v1;

    let home = tempfile::tempdir().expect("tempdir");
    let workspace = tempfile::tempdir().expect("tempdir");
    let server = MockInferenceServer::start().await;
    server.script(Endpoint::ChatCompletions, Reply::text("the first answer"));

    let agent = || {
        AcpAgent::new(
            AcpAgentConfig::new(env!("CARGO_BIN_EXE_keke"))
                .args(["agent", "stdio"])
                .env("KEKE_HOME", home.path().display().to_string())
                .env("KEKE_CREDENTIAL_STORE", "file")
                .env("KEKE_IMPORT", "off")
                .env("KEKE_PROVIDER", "grok")
                .env("KEKE_MODEL", "grok-4.6")
                .env("XAI_BASE_URL", server.base_url())
                .env("XAI_API_KEY", "test-key"),
        )
    };
    let cwd = workspace.path().to_path_buf();

    let started = agent_client_protocol::Client
        .builder()
        .connect_with(agent(), {
            let cwd = cwd.clone();
            |connection: ConnectionTo<Agent>| async move {
                connection
                    .send_request(v1::InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session = connection
                    .send_request(v1::NewSessionRequest::new(cwd))
                    .block_task()
                    .await?
                    .session_id;
                let reply = connection
                    .send_request(v1::PromptRequest::new(
                        session.clone(),
                        vec![v1::ContentBlock::Text(v1::TextContent::new(
                            "remember this",
                        ))],
                    ))
                    .block_task()
                    .await?;
                // The v1-shaped answer: v2 moved this onto the update stream.
                assert_eq!(reply.stop_reason, v1::StopReason::EndTurn);
                Ok(session)
            }
        })
        .await
        .expect("the v1 session runs to completion");

    let replayed = Arc::new(Mutex::new(Vec::<String>::new()));
    agent_client_protocol::Client
        .builder()
        .on_receive_notification(
            {
                let replayed = Arc::clone(&replayed);
                async move |notification: v1::SessionNotification, _cx| {
                    if let v1::SessionUpdate::UserMessageChunk(chunk)
                    | v1::SessionUpdate::AgentMessageChunk(chunk) = notification.update
                        && let v1::ContentBlock::Text(text) = chunk.content
                    {
                        replayed.lock().expect("lock").push(text.text);
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(agent(), {
            let cwd = cwd.clone();
            |connection: ConnectionTo<Agent>| async move {
                connection
                    .send_request(v1::InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                connection
                    .send_request(v1::LoadSessionRequest::new(started, cwd))
                    .block_task()
                    .await?;
                Ok(())
            }
        })
        .await
        .expect("the v1 load runs to completion");

    let replayed = replayed.lock().expect("lock");
    assert!(
        replayed.contains(&"remember this".to_string())
            && replayed.contains(&"the first answer".to_string()),
        "`session/load` replays both sides of the conversation: {replayed:?}"
    );
}
