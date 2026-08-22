use keke_protocol::ContentBlock;
use keke_protocol::Message;
use keke_protocol::Role;
use keke_protocol::StopReason;
use keke_protocol::ToolCall;
use keke_protocol::ToolCallId;
use keke_protocol::ToolResult;
use keke_protocol::Usage;
use keke_provider_api::ModelRequest;
use keke_provider_api::ProviderError;
use keke_provider_api::StreamChunk;
use keke_provider_api::ToolSpec;
use keke_provider_api::WireApi;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

use super::assert_ends_with_one_done;
use super::client_over;
use super::collect;
use super::collect_ok;
use super::one_tool_call;
use super::request;
use super::sent_body;
use super::sse;
use super::stream_response;

const API: WireApi = WireApi::Messages;

async fn serve(body: String) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(stream_response(body))
        .mount(&server)
        .await;
    server
}

fn stop(reason: &str) -> Vec<String> {
    vec![
        json!({"type":"message_delta","delta":{"stop_reason":reason},"usage":{"output_tokens":2}})
            .to_string(),
        json!({"type":"message_stop"}).to_string(),
    ]
}

#[tokio::test]
async fn text_deltas_assemble_and_end_with_one_done() {
    let mut frames = vec![
        json!({"type":"message_start","message":{"usage":{"input_tokens":7}}}).to_string(),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})
            .to_string(),
        json!({"type":"ping"}).to_string(),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}})
            .to_string(),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}})
            .to_string(),
        json!({"type":"content_block_stop","index":0}).to_string(),
    ];
    frames.extend(stop("end_turn"));
    let server = serve(sse(&frames)).await;
    let (client, _auth) = client_over(&server);

    let chunks = collect_ok(&client, API).await;

    assert_eq!(
        chunks,
        vec![
            StreamChunk::TextDelta("Hel".to_string()),
            StreamChunk::TextDelta("lo".to_string()),
            StreamChunk::Usage(Usage {
                input_tokens: 7,
                output_tokens: 2,
                ..Usage::default()
            }),
            StreamChunk::Done(StopReason::EndTurn),
        ]
    );
    assert_ends_with_one_done(&chunks);
}

#[tokio::test]
async fn a_tool_call_split_across_frames_reassembles() {
    let mut frames = vec![
        json!({"type":"message_start","message":{"usage":{"input_tokens":7}}}).to_string(),
        json!({"type":"content_block_start","index":0,"content_block":{
            "type":"tool_use","id":"toolu_1","name":"read_file","input":{}
        }})
        .to_string(),
        json!({"type":"content_block_delta","index":0,"delta":{
            "type":"input_json_delta","partial_json":"{\"path\":"
        }})
        .to_string(),
        json!({"type":"content_block_delta","index":0,"delta":{
            "type":"input_json_delta","partial_json":"\"a.rs\"}"
        }})
        .to_string(),
        json!({"type":"content_block_stop","index":0}).to_string(),
    ];
    frames.extend(stop("tool_use"));
    let server = serve(sse(&frames)).await;
    let (client, _auth) = client_over(&server);

    let chunks = collect_ok(&client, API).await;
    let (id, name, arguments) = one_tool_call(&chunks);

    assert_eq!(id, "toolu_1");
    assert_eq!(name, "read_file");
    assert_eq!(arguments, r#"{"path":"a.rs"}"#);
    assert_eq!(chunks.last(), Some(&StreamChunk::Done(StopReason::ToolUse)));
    assert_ends_with_one_done(&chunks);
}

#[tokio::test]
async fn a_stream_without_its_terminal_event_is_a_protocol_error() {
    let server = serve(sse(&[
        json!({"type":"message_start","message":{"usage":{"input_tokens":7}}}).to_string(),
        json!({"type":"content_block_delta","index":0,"delta":{
            "type":"text_delta","text":"half an answ"
        }})
        .to_string(),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}).to_string(),
    ]))
    .await;
    let (client, _auth) = client_over(&server);

    let chunks = collect(&client, API).await;

    // A `message_delta` naming a stop reason is not permission to report Done:
    // only `message_stop` ends the reply.
    assert!(
        !chunks
            .iter()
            .any(|chunk| matches!(chunk, Ok(StreamChunk::Done(_))))
    );
    assert!(
        matches!(chunks.last(), Some(Err(ProviderError::Protocol(_)))),
        "got {chunks:?}"
    );
}

#[tokio::test]
async fn usage_is_reported() {
    let server = serve(sse(&[
        json!({"type":"message_start","message":{"usage":{
            "input_tokens":7,"cache_read_input_tokens":4
        }}})
        .to_string(),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}})
            .to_string(),
        json!({"type":"message_stop"}).to_string(),
    ]))
    .await;
    let (client, _auth) = client_over(&server);

    let chunks = collect_ok(&client, API).await;

    // Input counts arrive at the start and output counts at the end, so the two
    // halves have to be merged rather than the later one replacing the earlier.
    assert!(chunks.contains(&StreamChunk::Usage(Usage {
        input_tokens: 7,
        output_tokens: 2,
        cached_input_tokens: 4,
        reasoning_tokens: 0,
    })));
    assert_ends_with_one_done(&chunks);
}

#[tokio::test]
async fn a_mid_stream_overload_is_transient() {
    let server = serve(sse(&[
        json!({"type":"message_start","message":{"usage":{"input_tokens":7}}}).to_string(),
        json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}})
            .to_string(),
    ]))
    .await;
    let (client, _auth) = client_over(&server);

    let chunks = collect(&client, API).await;

    assert!(
        matches!(chunks.last(), Some(Err(ProviderError::Transient(detail))) if detail.contains("Overloaded")),
        "got {chunks:?}"
    );
}

#[tokio::test]
async fn rate_limiting_carries_the_stated_delay_and_rejection_is_unauthorized() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "3")
                .set_body_string("slow down"),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(401).set_body_json(
            json!({"error": {"type": "authentication_error", "message": "invalid x-api-key"}}),
        ))
        .mount(&server)
        .await;
    let (client, _auth) = client_over(&server);

    let limited = client.stream(API, request()).await.err().expect("limited");
    assert!(
        matches!(
            limited,
            ProviderError::RateLimited {
                retry_after_millis: Some(3000)
            }
        ),
        "got {limited:?}"
    );

    let rejected = client.stream(API, request()).await.err().expect("rejected");
    assert!(
        matches!(rejected, ProviderError::Unauthorized(ref detail) if detail.contains("invalid x-api-key")),
        "got {rejected:?}"
    );
}

#[tokio::test]
async fn credentials_are_fetched_again_for_every_request() {
    let frames = sse(&stop("end_turn"));
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer token-0"))
        .respond_with(stream_response(frames.clone()))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer token-1"))
        .respond_with(stream_response(frames))
        .mount(&server)
        .await;
    let (client, auth) = client_over(&server);

    collect_ok(&client, API).await;
    collect_ok(&client, API).await;

    assert_eq!(auth.fetches(), 2);
}

#[tokio::test]
async fn the_system_prompt_is_a_top_level_field_not_a_message() {
    let server = serve(sse(&stop("end_turn"))).await;
    let (client, _auth) = client_over(&server);
    let call_id = ToolCallId::new("toolu_9");

    let sent = client
        .stream(
            API,
            ModelRequest {
                model: "a-model".to_string(),
                system: Some("be terse".to_string()),
                messages: vec![
                    Message {
                        role: Role::System,
                        content: vec![ContentBlock::text("and be kind")],
                    },
                    Message::user("read it"),
                    Message {
                        role: Role::Assistant,
                        content: vec![
                            ContentBlock::text("Looking."),
                            ContentBlock::ToolCall(ToolCall {
                                id: call_id.clone(),
                                name: "read_file".to_string(),
                                arguments: json!({"path": "a.rs"}),
                            }),
                        ],
                    },
                    Message {
                        role: Role::Tool,
                        content: vec![ContentBlock::ToolResult(ToolResult::ok(
                            call_id,
                            "fn main() {}",
                        ))],
                    },
                ],
                tools: vec![ToolSpec {
                    name: "read_file".to_string(),
                    description: "read a file".to_string(),
                    input_schema: json!({"type": "object"}),
                }],
                max_output_tokens: Some(256),
                ..ModelRequest::default()
            },
        )
        .await
        .expect("stream starts");
    drop(sent);
    let body = sent_body(&server).await;

    assert_eq!(body["system"], json!("be terse\n\nand be kind"));
    assert_eq!(body["max_tokens"], json!(256));
    assert_eq!(body["tools"][0]["input_schema"], json!({"type": "object"}));

    let messages = body["messages"].as_array().expect("messages");
    assert!(
        messages
            .iter()
            .all(|message| message["role"] != json!("system")),
        "a system message reached the array: {messages:?}"
    );
    assert_eq!(messages[0]["role"], json!("user"));
    assert_eq!(messages[0]["content"][0]["text"], json!("read it"));
    assert_eq!(messages[1]["role"], json!("assistant"));
    assert_eq!(messages[1]["content"][1]["type"], json!("tool_use"));
    // Arguments are an object here, not the JSON string the OpenAI wires use.
    assert_eq!(messages[1]["content"][1]["input"], json!({"path": "a.rs"}));
    assert_eq!(messages[2]["role"], json!("user"));
    assert_eq!(messages[2]["content"][0]["type"], json!("tool_result"));
    assert_eq!(messages[2]["content"][0]["tool_use_id"], json!("toolu_9"));
    assert_eq!(messages.len(), 3);
}

#[tokio::test]
async fn the_api_version_header_is_sent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(stream_response(sse(&stop("end_turn"))))
        .mount(&server)
        .await;
    let (client, _auth) = client_over(&server);

    assert_ends_with_one_done(&collect_ok(&client, API).await);
}

#[tokio::test]
async fn a_request_without_a_token_budget_still_names_one() {
    let server = serve(sse(&stop("end_turn"))).await;
    let (client, _auth) = client_over(&server);

    drop(client.stream(API, request()).await.expect("stream starts"));
    let body = sent_body(&server).await;

    // `max_tokens` is mandatory on this wire; omitting it is a 400.
    assert!(body["max_tokens"].as_u64().is_some_and(|max| max > 0));
}
