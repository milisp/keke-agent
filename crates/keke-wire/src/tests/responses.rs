use keke_protocol::ContentBlock;
use keke_protocol::Message;
use keke_protocol::ReasoningEffort;
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

const API: WireApi = WireApi::Responses;

async fn serve(body: String) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(stream_response(body))
        .mount(&server)
        .await;
    server
}

fn completed() -> String {
    json!({"type":"response.completed","response":{"status":"completed"}}).to_string()
}

#[tokio::test]
async fn text_deltas_assemble_and_end_with_one_done() {
    let server = serve(sse(&[
        json!({"type":"response.created"}).to_string(),
        json!({"type":"response.reasoning_summary_text.delta","delta":"pondering"}).to_string(),
        json!({"type":"response.output_text.delta","delta":"Hel"}).to_string(),
        json!({"type":"response.output_text.delta","delta":"lo"}).to_string(),
        completed(),
    ]))
    .await;
    let (client, _auth) = client_over(&server);

    let chunks = collect_ok(&client, API).await;

    assert_eq!(
        chunks,
        vec![
            StreamChunk::ThinkingDelta("pondering".to_string()),
            StreamChunk::TextDelta("Hel".to_string()),
            StreamChunk::TextDelta("lo".to_string()),
            StreamChunk::Done(StopReason::EndTurn),
        ]
    );
    assert_ends_with_one_done(&chunks);
}

#[tokio::test]
async fn a_tool_call_split_across_frames_reassembles() {
    let server = serve(sse(&[
        json!({"type":"response.output_item.added","output_index":0,"item":{
            "type":"function_call","id":"fc_1","call_id":"call_1","name":"read_file"
        }})
        .to_string(),
        json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"path\":"})
            .to_string(),
        json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"\"a.rs\"}"})
            .to_string(),
        json!({"type":"response.function_call_arguments.done","output_index":0}).to_string(),
        json!({"type":"response.output_item.done","output_index":0}).to_string(),
        completed(),
    ]))
    .await;
    let (client, _auth) = client_over(&server);

    let chunks = collect_ok(&client, API).await;
    let (id, name, arguments) = one_tool_call(&chunks);

    // `call_id` wins over the item's own `id`: it is what a result must echo.
    assert_eq!(id, "call_1");
    assert_eq!(name, "read_file");
    assert_eq!(arguments, r#"{"path":"a.rs"}"#);
    assert_eq!(chunks.last(), Some(&StreamChunk::Done(StopReason::ToolUse)));
    assert_ends_with_one_done(&chunks);
}

#[tokio::test]
async fn a_stream_that_stops_early_is_retryable_rather_than_malformed() {
    let server = serve(sse(&[
        json!({"type":"response.output_text.delta","delta":"half an answ"}).to_string(),
    ]))
    .await;
    let (client, _auth) = client_over(&server);

    let chunks = collect(&client, API).await;

    assert!(
        !chunks
            .iter()
            .any(|chunk| matches!(chunk, Ok(StreamChunk::Done(_))))
    );
    assert!(
        matches!(chunks.last(), Some(Err(ProviderError::Transient(_)))),
        "got {chunks:?}"
    );
}

#[tokio::test]
async fn usage_is_reported() {
    let server = serve(sse(&[
        json!({"type":"response.output_text.delta","delta":"hi"}).to_string(),
        json!({"type":"response.completed","response":{"status":"completed","usage":{
            "input_tokens":7,
            "output_tokens":2,
            "input_tokens_details":{"cached_tokens":4},
            "output_tokens_details":{"reasoning_tokens":1}
        }}})
        .to_string(),
    ]))
    .await;
    let (client, _auth) = client_over(&server);

    let chunks = collect_ok(&client, API).await;

    assert!(chunks.contains(&StreamChunk::Usage(Usage {
        input_tokens: 7,
        output_tokens: 2,
        cached_input_tokens: 4,
        reasoning_tokens: 1,
    })));
    assert_ends_with_one_done(&chunks);
}

#[tokio::test]
async fn a_failed_response_is_transient_not_a_silent_end() {
    let server = serve(sse(&[
        json!({"type":"response.output_text.delta","delta":"hi"}).to_string(),
        json!({"type":"response.failed","response":{"error":{"message":"upstream fell over"}}})
            .to_string(),
    ]))
    .await;
    let (client, _auth) = client_over(&server);

    let chunks = collect(&client, API).await;

    assert!(
        matches!(chunks.last(), Some(Err(ProviderError::Transient(detail))) if detail.contains("upstream fell over")),
        "got {chunks:?}"
    );
}

#[tokio::test]
async fn rate_limiting_carries_the_stated_delay_and_rejection_is_unauthorized() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "3")
                .set_body_string("slow down"),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({"error": "no access"})))
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
    assert!(matches!(rejected, ProviderError::Unauthorized(_)));
}

#[tokio::test]
async fn credentials_are_fetched_again_for_every_request() {
    let server = serve(sse(&[completed()])).await;
    let (client, auth) = client_over(&server);

    collect_ok(&client, API).await;
    collect_ok(&client, API).await;

    assert_eq!(auth.fetches(), 2);
    let requests = server.received_requests().await.expect("recorded");
    let sent: Vec<_> = requests
        .iter()
        .map(|request| {
            request.headers["authorization"]
                .to_str()
                .expect("ascii")
                .to_string()
        })
        .collect();
    assert_eq!(sent, vec!["Bearer token-0", "Bearer token-1"]);
}

#[tokio::test]
async fn the_conversation_is_sent_as_input_items() {
    let server = serve(sse(&[completed()])).await;
    let (client, _auth) = client_over(&server);
    let call_id = ToolCallId::new("call_9");

    let sent = client
        .stream(
            API,
            ModelRequest {
                model: "a-model".to_string(),
                system: Some("be terse".to_string()),
                messages: vec![
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

    // No `messages` array at all, and the system prompt is `instructions`.
    assert!(body.get("messages").is_none());
    assert_eq!(body["instructions"], json!("be terse"));
    assert_eq!(body["max_output_tokens"], json!(256));
    // Tools are flat here: no `function` envelope.
    assert_eq!(body["tools"][0]["name"], json!("read_file"));

    let input = body["input"].as_array().expect("input items");
    assert_eq!(input[0]["type"], json!("message"));
    assert_eq!(input[0]["content"][0]["type"], json!("input_text"));
    assert_eq!(input[1]["type"], json!("message"));
    assert_eq!(input[1]["content"][0]["type"], json!("output_text"));
    assert_eq!(input[2]["type"], json!("function_call"));
    assert_eq!(input[2]["call_id"], json!("call_9"));
    assert_eq!(input[2]["arguments"], json!(r#"{"path":"a.rs"}"#));
    assert_eq!(input[3]["type"], json!("function_call_output"));
    assert_eq!(input[3]["call_id"], json!("call_9"));
    assert_eq!(input[3]["output"], json!("fn main() {}"));
    assert_eq!(input.len(), 4);
}

/// This wire nests the level under `reasoning`, and takes it as written.
#[test]
fn effort_is_nested_under_reasoning() {
    let body = crate::responses_body(
        &ModelRequest {
            reasoning_effort: Some(ReasoningEffort::Max),
            ..request()
        },
        false,
    );
    assert_eq!(body["reasoning"], json!({"effort": "max"}));
}

#[test]
fn an_unset_effort_leaves_the_field_off() {
    let body = crate::responses_body(&request(), false);
    assert!(body.get("reasoning").is_none(), "{body}");
}
