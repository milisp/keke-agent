//! These assert wire bytes rather than a decoded shape: the mock's job is to
//! reproduce what a vendor actually sends, and a test that decodes first would
//! pass even if the framing were wrong.

use std::error::Error;

use keke_test_support::Endpoint;
use keke_test_support::MockInferenceServer;
use keke_test_support::Reply;
use keke_test_support::SseFrame;
use keke_test_support::Stop;
use serde_json::Value;
use serde_json::json;

type Result = std::result::Result<(), Box<dyn Error>>;

async fn post_stream(server: &MockInferenceServer, endpoint: Endpoint, body: Value) -> String {
    let url = format!("{}{}", server.origin(), endpoint.path());
    match reqwest::Client::new().post(url).json(&body).send().await {
        Ok(response) => response.text().await.unwrap_or_default(),
        Err(error) => panic!("mock request failed: {error}"),
    }
}

fn streaming_body() -> Value {
    json!({ "model": "mock-model", "stream": true, "messages": [] })
}

/// `data:` payloads in arrival order, event names dropped.
fn data_frames(wire: &str) -> Vec<&str> {
    wire.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .collect()
}

fn event_names(wire: &str) -> Vec<&str> {
    wire.lines()
        .filter_map(|line| line.strip_prefix("event: "))
        .collect()
}

#[tokio::test]
async fn chat_completions_streams_text_as_deltas_terminated_by_done() -> Result {
    let server = MockInferenceServer::start().await;
    server.script(Endpoint::ChatCompletions, Reply::text("hello"));

    let wire = post_stream(&server, Endpoint::ChatCompletions, streaming_body()).await;
    let frames = data_frames(&wire);

    assert!(
        wire.starts_with("data: "),
        "chat completions frames are unnamed: {wire}"
    );
    assert_eq!(frames.last(), Some(&"[DONE]"));
    let content: String = frames
        .iter()
        .filter_map(|frame| serde_json::from_str::<Value>(frame).ok())
        .filter_map(|chunk| {
            chunk["choices"][0]["delta"]["content"]
                .as_str()
                .map(str::to_owned)
        })
        .collect();
    assert_eq!(content, "hello");

    let finish: Vec<Value> = frames
        .iter()
        .filter_map(|frame| serde_json::from_str::<Value>(frame).ok())
        .map(|chunk| chunk["choices"][0]["finish_reason"].clone())
        .filter(|reason| !reason.is_null())
        .collect();
    assert_eq!(finish, vec![json!("stop")]);
    Ok(())
}

#[tokio::test]
async fn responses_streams_text_as_typed_events_ending_with_response_completed() -> Result {
    let server = MockInferenceServer::start().await;
    server.script(Endpoint::Responses, Reply::text("hi").with_usage(100, 20));

    let wire = post_stream(&server, Endpoint::Responses, streaming_body()).await;
    let names = event_names(&wire);

    assert_eq!(names.first(), Some(&"response.created"));
    assert_eq!(names.last(), Some(&"response.completed"));
    assert!(names.contains(&"response.output_text.delta"), "{names:?}");
    assert!(
        !wire.contains("[DONE]"),
        "the Responses API has no [DONE] sentinel"
    );

    let last: Value = serde_json::from_str(data_frames(&wire).last().ok_or("no frames")?)?;
    assert_eq!(last["response"]["usage"]["input_tokens"], json!(100));
    assert_eq!(last["response"]["usage"]["output_tokens"], json!(20));
    assert_eq!(
        last["response"]["output"][0]["content"][0]["text"],
        json!("hi")
    );
    Ok(())
}

#[tokio::test]
async fn messages_streams_text_as_typed_events_ending_with_message_stop() -> Result {
    let server = MockInferenceServer::start().await;
    server.script(Endpoint::Messages, Reply::text("hey"));

    let wire = post_stream(&server, Endpoint::Messages, streaming_body()).await;

    assert_eq!(
        event_names(&wire),
        vec![
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ]
    );
    let delta: Value = serde_json::from_str(data_frames(&wire).get(2).ok_or("no delta frame")?)?;
    assert_eq!(delta["delta"]["type"], json!("text_delta"));
    assert_eq!(delta["delta"]["text"], json!("hey"));
    Ok(())
}

#[tokio::test]
async fn one_reply_renders_into_all_three_wire_formats() -> Result {
    let server = MockInferenceServer::start().await;
    let reply = || {
        Reply::thinking("because")
            .with_text("done")
            .with_tool_call("read_file", json!({ "path": "a" }))
    };
    for endpoint in [
        Endpoint::ChatCompletions,
        Endpoint::Responses,
        Endpoint::Messages,
    ] {
        server.script(endpoint, reply());
        let wire = post_stream(&server, endpoint, streaming_body()).await;
        assert!(
            wire.contains("because"),
            "{endpoint:?} lost the reasoning: {wire}"
        );
        assert!(wire.contains("done"), "{endpoint:?} lost the text: {wire}");
        assert!(
            wire.contains("read_file"),
            "{endpoint:?} lost the tool call: {wire}"
        );
    }

    // Each format spells the same "stopped to call a tool" its own way.
    Ok(())
}

#[tokio::test]
async fn a_tool_call_stops_the_turn_for_tool_use_in_every_format() -> Result {
    let server = MockInferenceServer::start().await;
    let reply = || Reply::tool_call("read_file", json!({ "path": "a" }));

    server.script(Endpoint::ChatCompletions, reply());
    let chat = post_stream(&server, Endpoint::ChatCompletions, streaming_body()).await;
    assert!(chat.contains(r#""finish_reason":"tool_calls""#), "{chat}");

    server.script(Endpoint::Messages, reply());
    let messages = post_stream(&server, Endpoint::Messages, streaming_body()).await;
    assert!(
        messages.contains(r#""stop_reason":"tool_use""#),
        "{messages}"
    );

    server.script(Endpoint::Responses, reply());
    let responses = post_stream(&server, Endpoint::Responses, streaming_body()).await;
    assert!(
        responses.contains("response.function_call_arguments.done"),
        "{responses}"
    );
    Ok(())
}

#[tokio::test]
async fn tool_call_arguments_arrive_split_across_frames() -> Result {
    let server = MockInferenceServer::start().await;
    server.script(
        Endpoint::ChatCompletions,
        Reply::tool_call("read_file", json!({ "path": "a" })),
    );

    let wire = post_stream(&server, Endpoint::ChatCompletions, streaming_body()).await;
    let calls: Vec<Value> = data_frames(&wire)
        .iter()
        .filter_map(|frame| serde_json::from_str::<Value>(frame).ok())
        .filter(|chunk| !chunk["choices"][0]["delta"]["tool_calls"].is_null())
        .collect();

    assert_eq!(
        calls[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
        json!("read_file")
    );
    let fragments: Vec<String> = calls
        .iter()
        .skip(1)
        .filter_map(|chunk| {
            chunk["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .map(str::to_owned)
        })
        .collect();
    assert!(
        fragments.len() > 1,
        "arguments must span frames: {fragments:?}"
    );
    assert_eq!(
        serde_json::from_str::<Value>(&fragments.concat())?,
        json!({ "path": "a" })
    );
    Ok(())
}

#[tokio::test]
async fn a_scripted_status_carries_its_headers() -> Result {
    let server = MockInferenceServer::start().await;
    server.script(
        Endpoint::ChatCompletions,
        Reply::status(429).with_header("retry-after", "3"),
    );

    let response = reqwest::Client::new()
        .post(format!("{}/chat/completions", server.base_url()))
        .json(&streaming_body())
        .send()
        .await?;

    assert_eq!(response.status().as_u16(), 429);
    assert_eq!(
        response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok()),
        Some("3")
    );
    let body: Value = response.json().await?;
    assert_eq!(body["error"]["code"], json!(429));
    Ok(())
}

#[tokio::test]
async fn a_truncated_reply_ends_without_its_terminal_frame() -> Result {
    let server = MockInferenceServer::start().await;
    for endpoint in [
        Endpoint::ChatCompletions,
        Endpoint::Responses,
        Endpoint::Messages,
    ] {
        server.script(endpoint, Reply::text("half a th").truncated());
        let wire = post_stream(&server, endpoint, streaming_body()).await;
        assert!(wire.contains("half a th"), "{endpoint:?}: {wire}");
        for terminal in [
            "[DONE]",
            "response.completed",
            "message_stop",
            "finish_reason\":\"",
        ] {
            assert!(
                !wire.contains(terminal),
                "{endpoint:?} still sent {terminal}: {wire}"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn raw_sse_is_served_verbatim() -> Result {
    let server = MockInferenceServer::start().await;
    server.script(
        Endpoint::ChatCompletions,
        Reply::raw_sse(vec![
            SseFrame::data("{ not json"),
            SseFrame::named("weird", "{}"),
        ]),
    );

    let wire = post_stream(&server, Endpoint::ChatCompletions, streaming_body()).await;
    assert_eq!(wire, "data: { not json\n\nevent: weird\ndata: {}\n\n");
    Ok(())
}

#[tokio::test]
async fn a_non_streaming_request_gets_one_json_body() -> Result {
    let server = MockInferenceServer::start().await;
    server.script(
        Endpoint::ChatCompletions,
        Reply::text("hello").with_stop(Stop::MaxTokens),
    );

    let body: Value = reqwest::Client::new()
        .post(format!("{}/chat/completions", server.base_url()))
        .json(&json!({ "model": "mock-model", "messages": [] }))
        .send()
        .await?
        .json()
        .await?;

    assert_eq!(body["object"], json!("chat.completion"));
    assert_eq!(body["choices"][0]["message"]["content"], json!("hello"));
    assert_eq!(body["choices"][0]["finish_reason"], json!("length"));
    Ok(())
}

#[tokio::test]
async fn the_request_log_records_the_body_and_the_authorization_header() -> Result {
    let server = MockInferenceServer::start().await;
    server.script(Endpoint::Messages, Reply::text("ok"));

    reqwest::Client::new()
        .post(format!("{}/messages", server.base_url()))
        .bearer_auth("sk-test")
        .json(
            &json!({ "model": "claude-mock", "stream": true, "messages": [
            { "role": "user", "content": "ping" }
        ] }),
        )
        .send()
        .await?
        .text()
        .await?;

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].endpoint, Endpoint::Messages);
    assert_eq!(requests[0].path, "/v1/messages");
    assert_eq!(requests[0].authorization(), Some("Bearer sk-test"));
    assert_eq!(requests[0].model(), Some("claude-mock"));
    assert_eq!(requests[0].body["messages"][0]["content"], json!("ping"));
    Ok(())
}

#[tokio::test]
async fn the_request_log_is_readable_while_the_server_runs() -> Result {
    let server = std::sync::Arc::new(MockInferenceServer::start().await);
    server.script(Endpoint::ChatCompletions, Reply::text("one"));

    let reader = {
        let server = server.clone();
        tokio::spawn(async move {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while server.request_count() == 0 && std::time::Instant::now() < deadline {
                tokio::task::yield_now().await;
            }
            server.requests()
        })
    };

    post_stream(&server, Endpoint::ChatCompletions, streaming_body()).await;
    let seen = reader.await?;
    assert_eq!(seen.len(), 1);
    Ok(())
}

#[tokio::test]
async fn an_unscripted_endpoint_answers_legibly_instead_of_hanging() -> Result {
    let server = MockInferenceServer::start().await;

    let wire = post_stream(
        &server,
        Endpoint::ChatCompletions,
        json!({ "model": "mock-model", "stream": true, "messages": [
            { "role": "user", "content": "ping" }
        ] }),
    )
    .await;

    assert!(
        wire.contains("nothing scripted for /v1/chat/completions"),
        "{wire}"
    );
    assert!(wire.contains("ping"), "{wire}");
    assert!(wire.contains("[DONE]"), "{wire}");
    Ok(())
}

#[tokio::test]
async fn models_lists_what_it_was_given() -> Result {
    let server = MockInferenceServer::start().await;
    server.set_models(["a", "b"]);

    let body: Value = reqwest::get(format!("{}/models", server.base_url()))
        .await?
        .json()
        .await?;

    assert_eq!(body["data"][0]["id"], json!("a"));
    assert_eq!(body["data"][1]["id"], json!("b"));
    assert_eq!(server.requests_to(Endpoint::Models).len(), 1);
    Ok(())
}

#[tokio::test]
async fn scripts_are_consumed_in_order() -> Result {
    let server = MockInferenceServer::start().await;
    server.script(Endpoint::ChatCompletions, Reply::text("first"));
    server.script(Endpoint::ChatCompletions, Reply::text("second"));
    assert_eq!(server.pending_scripts(Endpoint::ChatCompletions), 2);

    let first = post_stream(&server, Endpoint::ChatCompletions, streaming_body()).await;
    let second = post_stream(&server, Endpoint::ChatCompletions, streaming_body()).await;

    assert!(first.contains("first") && !first.contains("second"));
    assert!(second.contains("second"));
    assert_eq!(server.pending_scripts(Endpoint::ChatCompletions), 0);
    Ok(())
}

#[tokio::test]
async fn dropping_the_server_frees_its_port() -> Result {
    let server = MockInferenceServer::start().await;
    let addr = server.addr();
    server.script(Endpoint::ChatCompletions, Reply::text("hello"));
    post_stream(&server, Endpoint::ChatCompletions, streaming_body()).await;
    drop(server);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(_) => return Ok(()),
            Err(error) if std::time::Instant::now() >= deadline => {
                return Err(format!("port {addr} never freed: {error}").into());
            }
            Err(_) => tokio::task::yield_now().await,
        }
    }
}
