//! Images remain model-visible and tied to their originating tool calls.

use keke_protocol::{ContentBlock, ImageBlock, Message, Role, ToolCall, ToolCallId, ToolResult};
use serde_json::{Value, json};

fn image(data: &str) -> ContentBlock {
    ContentBlock::Image(ImageBlock {
        data: data.to_string(),
        media_type: "image/png".to_string(),
    })
}

fn request(images: bool) -> keke_provider_api::ModelRequest {
    let mut request = super::request();
    request.messages = vec![
        Message {
            role: Role::Assistant,
            content: ["first", "second"]
                .into_iter()
                .map(|id| {
                    ContentBlock::ToolCall(ToolCall {
                        id: ToolCallId::new(id),
                        name: "view_image".to_string(),
                        arguments: json!({"path": id}),
                    })
                })
                .collect(),
        },
        Message {
            role: Role::Tool,
            content: ["first", "second"]
                .into_iter()
                .map(|id| {
                    let mut result = ToolResult::ok(ToolCallId::new(id), format!("{id} before"));
                    if images {
                        result.content.extend([
                            image(&format!("{id}-a")),
                            ContentBlock::text("between"),
                            image(&format!("{id}-b")),
                        ]);
                    }
                    ContentBlock::ToolResult(result)
                })
                .collect(),
        },
    ];
    request
}

#[test]
fn text_only_tool_results_keep_their_string_shape_on_every_wire() {
    let request = request(false);
    let chat = crate::chat_completions_body(&request, false);
    assert_eq!(chat["messages"].as_array().unwrap().len(), 3);
    assert_eq!(chat["messages"][1]["content"], "first before");
    assert_eq!(chat["messages"][2]["content"], "second before");
    let responses = crate::responses_body(&request, false, true);
    assert_eq!(responses["input"][2]["output"], "first before");
    assert_eq!(responses["input"][3]["output"], "second before");
    let messages = crate::messages_body(&request, false);
    assert_eq!(
        messages["messages"][1]["content"][0]["content"],
        "first before"
    );
    assert_eq!(
        messages["messages"][1]["content"][1]["content"],
        "second before"
    );
}

#[test]
fn responses_tool_batch_keeps_image_order_and_call_mapping() {
    let body = crate::responses_body(&request(true), false, true);
    for (index, id) in [(2, "first"), (3, "second")] {
        assert_eq!(body["input"][index]["call_id"], id);
        assert_eq!(
            body["input"][index]["output"],
            json!([
                {"type":"input_text", "text": format!("{id} before")},
                {"type":"input_image", "image_url": format!("data:image/png;base64,{id}-a")},
                {"type":"input_text", "text": "between"},
                {"type":"input_image", "image_url": format!("data:image/png;base64,{id}-b")},
            ])
        );
    }
}

#[test]
fn messages_tool_batch_keeps_image_order_and_call_mapping() {
    let body = crate::messages_body(&request(true), false);
    for (index, id) in [(0, "first"), (1, "second")] {
        let result = &body["messages"][1]["content"][index];
        assert_eq!(result["tool_use_id"], id);
        assert_eq!(
            result["content"],
            json!([
                {"type":"text", "text":format!("{id} before")},
                {"type":"image", "source":{"type":"base64", "media_type":"image/png", "data":format!("{id}-a")}},
                {"type":"text", "text":"between"},
                {"type":"image", "source":{"type":"base64", "media_type":"image/png", "data":format!("{id}-b")}},
            ])
        );
    }
}

#[test]
fn chat_answers_every_call_before_sending_labelled_tool_images() {
    let body = crate::chat_completions_body(&request(true), false);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[1]["tool_call_id"], "first");
    assert_eq!(messages[2]["tool_call_id"], "second");
    assert_eq!(messages[3]["role"], "user");
    let content = messages[3]["content"].as_array().unwrap();
    assert_eq!(content.len(), 8);
    for (index, id, suffix) in [
        (0, "first", "a"),
        (2, "first", "b"),
        (4, "second", "a"),
        (6, "second", "b"),
    ] {
        assert_eq!(
            content[index]["text"],
            format!("Image returned by tool call {id}:")
        );
        assert_eq!(
            content[index + 1]["image_url"]["url"],
            format!("data:image/png;base64,{id}-{suffix}")
        );
    }
    assert!(messages[1]["content"].is_string());
    assert!(messages[2]["content"].is_string());
}

#[test]
fn user_images_remain_in_order_on_every_wire() {
    let mut request = super::request();
    request.messages = vec![Message {
        role: Role::User,
        content: vec![
            ContentBlock::text("before"),
            image("one"),
            ContentBlock::text("between"),
            image("two"),
        ],
    }];
    let bodies: [Value; 3] = [
        crate::chat_completions_body(&request, false),
        crate::responses_body(&request, false, true),
        crate::messages_body(&request, false),
    ];
    for (index, body) in bodies.iter().enumerate() {
        let blocks = if index == 1 {
            &body["input"][0]["content"]
        } else {
            &body["messages"][0]["content"]
        };
        assert_eq!(blocks.as_array().unwrap().len(), 4);
        assert_eq!(blocks[0]["text"], "before");
        assert_eq!(blocks[2]["text"], "between");
        for (position, data) in [(1, "one"), (3, "two")] {
            match index {
                0 => assert_eq!(
                    blocks[position]["image_url"]["url"],
                    format!("data:image/png;base64,{data}")
                ),
                1 => assert_eq!(
                    blocks[position]["image_url"],
                    format!("data:image/png;base64,{data}")
                ),
                _ => assert_eq!(
                    blocks[position]["source"],
                    json!({"type":"base64", "media_type":"image/png", "data":data})
                ),
            }
        }
    }
}
