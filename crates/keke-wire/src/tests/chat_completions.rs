use keke_protocol::ReasoningEffort;
use keke_protocol::StopReason;
use keke_protocol::Usage;
use keke_provider_api::ModelRequest;
use keke_provider_api::ProviderError;
use keke_provider_api::StreamChunk;
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

const API: WireApi = WireApi::ChatCompletions;

async fn serve(body: String) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(stream_response(body))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn text_deltas_assemble_and_end_with_one_done() {
    let server = serve(sse(&[
        json!({"choices":[{"delta":{"content":"Hel"}}]}).to_string(),
        json!({"choices":[{"delta":{"reasoning_content":"pondering"}}]}).to_string(),
        json!({"choices":[{"delta":{"content":"lo"}}]}).to_string(),
        json!({"choices":[{"delta":{},"finish_reason":"stop"}]}).to_string(),
        "[DONE]".to_string(),
    ]))
    .await;
    let (client, _auth) = client_over(&server);

    let chunks = collect_ok(&client, API).await;

    assert_eq!(
        chunks,
        vec![
            StreamChunk::TextDelta("Hel".to_string()),
            StreamChunk::ThinkingDelta("pondering".to_string()),
            StreamChunk::TextDelta("lo".to_string()),
            StreamChunk::Done(StopReason::EndTurn),
        ]
    );
    assert_ends_with_one_done(&chunks);
}

#[tokio::test]
async fn a_tool_call_split_across_frames_reassembles() {
    let server = serve(sse(&[
        json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"id":"call_1","type":"function","function":{"name":"read_file","arguments":""}}
        ]}}]})
        .to_string(),
        json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"function":{"arguments":"{\"path\":"}}
        ]}}]})
        .to_string(),
        json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"function":{"arguments":"\"a.rs\"}"}}
        ]}}]})
        .to_string(),
        json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}).to_string(),
        "[DONE]".to_string(),
    ]))
    .await;
    let (client, _auth) = client_over(&server);

    let chunks = collect_ok(&client, API).await;
    let (id, name, arguments) = one_tool_call(&chunks);

    assert_eq!(id, "call_1");
    assert_eq!(name, "read_file");
    assert_eq!(arguments, r#"{"path":"a.rs"}"#);
    assert_eq!(chunks.last(), Some(&StreamChunk::Done(StopReason::ToolUse)));
    assert_ends_with_one_done(&chunks);
}

#[tokio::test]
async fn a_stream_that_stops_early_is_retryable_rather_than_malformed() {
    let server = serve(sse(&[
        json!({"choices":[{"delta":{"content":"half an answ"}}]}).to_string(),
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
        json!({"choices":[{"delta":{},"finish_reason":"stop"}]}).to_string(),
        json!({"choices":[],"usage":{
            "prompt_tokens":7,
            "completion_tokens":2,
            "prompt_tokens_details":{"cached_tokens":4},
            "completion_tokens_details":{"reasoning_tokens":1}
        }})
        .to_string(),
        "[DONE]".to_string(),
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
async fn rate_limiting_carries_the_stated_delay_and_rejection_is_unauthorized() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "3")
                .set_body_string("slow down"),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_json(json!({"error": {"message": "Incorrect API key"}})),
        )
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
        matches!(rejected, ProviderError::Unauthorized(ref detail) if detail.contains("Incorrect API key")),
        "got {rejected:?}"
    );
    assert!(rejected.needs_reauth());
}

#[tokio::test]
async fn credentials_are_fetched_again_for_every_request() {
    let frames = sse(&[
        json!({"choices":[{"delta":{},"finish_reason":"stop"}]}).to_string(),
        "[DONE]".to_string(),
    ]);
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
async fn the_system_prompt_leads_the_message_array() {
    let server = serve(sse(&[
        json!({"choices":[{"delta":{},"finish_reason":"stop"}]}).to_string(),
        "[DONE]".to_string(),
    ]))
    .await;
    let (client, _auth) = client_over(&server);
    let mut request = request();
    request.system = Some("be terse".to_string());

    drop(client.stream(API, request).await.expect("stream starts"));
    let body = sent_body(&server).await;

    assert_eq!(body["messages"][0]["role"], json!("system"));
    assert_eq!(body["messages"][0]["content"], json!("be terse"));
    assert_eq!(body["stream_options"]["include_usage"], json!(true));
    assert!(body.get("input").is_none());
}

/// The effort level is a top-level word on this wire, and an unknown rung is
/// the endpoint's to refuse — translating `xhigh` down to `high` would buy less
/// thinking than was asked for without saying so.
#[test]
fn effort_is_sent_as_written() {
    let body = crate::chat_completions_body(
        &ModelRequest {
            reasoning_effort: Some(ReasoningEffort::XHigh),
            ..request()
        },
        false,
    );
    assert_eq!(body["reasoning_effort"], json!("xhigh"));
}

#[test]
fn an_unset_effort_leaves_the_field_off() {
    let body = crate::chat_completions_body(&request(), false);
    assert!(body.get("reasoning_effort").is_none(), "{body}");
}
