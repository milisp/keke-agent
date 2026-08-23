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
use agent_client_protocol::schema::v1::ContentBlock;
use agent_client_protocol::schema::v1::InitializeRequest;
use agent_client_protocol::schema::v1::NewSessionRequest;
use agent_client_protocol::schema::v1::PromptRequest;
use agent_client_protocol::schema::v1::RequestPermissionOutcome;
use agent_client_protocol::schema::v1::RequestPermissionRequest;
use agent_client_protocol::schema::v1::RequestPermissionResponse;
use agent_client_protocol::schema::v1::SelectedPermissionOutcome;
use agent_client_protocol::schema::v1::SessionNotification;
use agent_client_protocol::schema::v1::SessionUpdate;
use agent_client_protocol::schema::v1::StopReason;
use agent_client_protocol::schema::v1::TextContent;
use agent_client_protocol::schema::v1::ToolCallStatus;
use keke_test_support::Endpoint;
use keke_test_support::MockInferenceServer;
use keke_test_support::Reply;

/// What the client saw, in arrival order.
#[derive(Default)]
struct Seen {
    text: String,
    tool_calls: Vec<String>,
    tool_statuses: Vec<ToolCallStatus>,
    permissions: Vec<String>,
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
        .builder()
        .on_receive_notification(
            {
                let seen = Arc::clone(&seen);
                async move |notification: SessionNotification, _cx| {
                    let mut seen = seen.lock().expect("lock");
                    match notification.update {
                        SessionUpdate::AgentMessageChunk(chunk) => {
                            if let ContentBlock::Text(text) = chunk.content {
                                seen.text.push_str(&text.text);
                            }
                        }
                        SessionUpdate::ToolCall(call) => seen.tool_calls.push(call.title),
                        SessionUpdate::ToolCallUpdate(update) => {
                            if let Some(status) = update.fields.status {
                                seen.tool_statuses.push(status);
                            }
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
                        .push(request.tool_call.fields.title.clone().unwrap_or_default());
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
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;

                let session = connection
                    .send_request(NewSessionRequest::new(cwd))
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
                assert_eq!(reply.stop_reason, StopReason::EndTurn);
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
        vec![ToolCallStatus::Completed],
        "an approved call must not come back failed"
    );
    assert_eq!(
        seen.permissions.len(),
        1,
        "the editor must have been asked exactly once: {:?}",
        seen.permissions
    );
}
