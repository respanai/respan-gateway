use super::*;

use crate::anthropic::AnthropicMessages;
use crate::openai::OpenAiChat;
use respan_gateway_conversation::{Block, Message, ResponseFormat, Role, Tool, ToolChoice};
use respan_gateway_core::{NativeFields, NativeValue};
use respan_gateway_http::bytes::{Bytes, BytesMut};
use respan_gateway_http::{Decoder, Sse};
use respan_gateway_sse::event_json;

fn decode_request(v: &Value) -> Input {
    let mut chat = OpenAiResponses.decode_json(v);
    chat.model = v["model"].as_str().map(Into::into);
    chat
}

/// A request through the kind and back.
fn round_trip(v: &Value) -> Value {
    let chat = decode_request(v);
    Value::Object(OpenAiResponses.encode_request(&chat).expect("a request"))
}

/// `events` as the SSE frames a provider sends.
fn frames(events: &[Value]) -> Vec<String> {
    let named = |e: &Value| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap_or(""));
    events.iter().map(named).collect()
}

/// The events of a stream of `events`, and how it ended.
fn decode_stream(events: &[Value]) -> (Vec<Output>, Result<(), Fault>) {
    let mut d = Sse::new(ResponsesDecoder::default());
    let mut out = Vec::new();
    for frame in frames(events) {
        if let Err(f) = d.decode(Bytes::from(frame), &mut out) {
            return (out, Err(f));
        }
    }
    let end = d.end(Ok(()), &mut out);
    (out, end)
}

fn decode_body(v: &Value) -> Vec<Output> {
    let mut out = Vec::new();
    ResponsesDecoder::default()
        .body(v, &mut out)
        .expect("a whole response");
    out
}

/// `replies` as a stream, whole, and the JSON of its events.
fn stream_of(replies: &[Output]) -> Result<Vec<Value>, Fault> {
    let mut enc = encode::Events::default();
    let mut out = BytesMut::new();
    for reply in replies {
        enc.encode_into(reply, &mut out)?;
    }
    enc.end_into(&Ok(()), &mut out)?;
    let text = String::from_utf8_lossy(&out).into_owned();
    Ok(text
        .split("\n\n")
        .filter(|e| !e.is_empty())
        .filter_map(|e| event_json(e.as_bytes()))
        .collect())
}

fn body_of(replies: &[Output]) -> Result<Value, Fault> {
    let mut whole = Assembled::default();
    replies.iter().try_for_each(|r| whole.push(r))?;
    OpenAiResponses.encode_whole(&whole)
}

fn types(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .map(|e| e["type"].as_str().unwrap_or(""))
        .collect()
}

fn call(id: &str, name: &str) -> Block {
    Block::ToolCall {
        id: id.into(),
        name: name.into(),
    }
}

fn response(status: &str, output: Value) -> Value {
    json!({
        "id": "resp_1", "object": "response", "created_at": 5, "status": status,
        "model": "gpt-x", "output": output, "usage": null,
        "parallel_tool_calls": true, "tool_choice": "auto", "tools": [],
    })
}

/// A documented stream of one message that says `Hello world`.
fn text_stream() -> Vec<Value> {
    let item = |status: &str, content: Value| json!({ "id": "msg_1", "type": "message", "status": status, "role": "assistant", "content": content });
    let part = |t: &str| json!({ "type": "output_text", "text": t, "annotations": [] });
    let mut done = response(
        "completed",
        json!([item("completed", json!([part("Hello world")]))]),
    );
    done["usage"] = json!({
        "input_tokens": 5, "input_tokens_details": { "cached_tokens": 2 },
        "output_tokens": 3, "output_tokens_details": { "reasoning_tokens": 1 },
        "total_tokens": 8,
    });
    vec![
        json!({ "type": "response.created", "response": response("in_progress", json!([])) }),
        json!({ "type": "response.in_progress", "response": response("in_progress", json!([])) }),
        json!({ "type": "response.output_item.added", "output_index": 0, "item": item("in_progress", json!([])) }),
        json!({ "type": "response.content_part.added", "item_id": "msg_1", "output_index": 0, "content_index": 0, "part": part("") }),
        json!({ "type": "response.output_text.delta", "item_id": "msg_1", "output_index": 0, "content_index": 0, "delta": "Hello", "logprobs": [] }),
        json!({ "type": "response.output_text.delta", "item_id": "msg_1", "output_index": 0, "content_index": 0, "delta": " world", "logprobs": [] }),
        json!({ "type": "response.output_text.done", "item_id": "msg_1", "output_index": 0, "content_index": 0, "text": "Hello world", "logprobs": [] }),
        json!({ "type": "response.content_part.done", "item_id": "msg_1", "output_index": 0, "content_index": 0, "part": part("Hello world") }),
        json!({ "type": "response.output_item.done", "output_index": 0, "item": item("completed", json!([part("Hello world")])) }),
        json!({ "type": "response.completed", "response": done }),
    ]
}

// ------------------------------------------------------------------ request

#[test]
fn a_request_reads_as_the_kind_and_writes_as_it_was() {
    let v = json!({
        "model": "gpt-x",
        "instructions": "be brief",
        "input": [
            { "role": "user", "content": "hi" },
            { "role": "assistant", "content": "hello" },
            { "role": "user", "content": [
                { "type": "input_text", "text": "look" },
                { "type": "input_image", "image_url": "https://x.test/a.png", "detail": "auto" },
            ] },
        ],
        "max_output_tokens": 50,
        "temperature": 0.5,
        "top_p": 0.9,
        "user": "u1",
        "tools": [{ "type": "function", "name": "f", "description": "d", "parameters": { "type": "object" }, "strict": true }],
        "tool_choice": { "type": "function", "name": "f" },
        "parallel_tool_calls": false,
        "text": { "format": { "type": "json_schema", "name": "n", "schema": { "type": "object" }, "strict": true } },
    });
    let chat = decode_request(&v);
    assert_eq!(chat.model.as_deref(), Some("gpt-x"));
    assert_eq!(chat.messages.len(), 4);
    assert_eq!(chat.messages[0].role, Role::System);
    assert_eq!(chat.messages[0].text(), "be brief");
    assert_eq!(chat.options.max_output_tokens, Some(50));
    assert_eq!(chat.options.tool_choice, Some(ToolChoice::Tool("f".into())));
    assert_eq!(chat.options.parallel_tool_calls, Some(false));
    assert_eq!(chat.user.as_deref(), Some("u1"));
    assert!(matches!(
        chat.options.response_format,
        Some(ResponseFormat::JsonSchema { .. })
    ));
    assert_eq!(chat.tools[0].name, "f");
    assert!(
        chat.native.is_none(),
        "everything is the kind's: {:?}",
        chat.native
    );
    assert_eq!(round_trip(&v), v);
}

#[test]
fn a_string_input_is_one_user_message() {
    let chat = decode_request(&json!({ "model": "m", "input": "hi" }));
    assert_eq!(chat.messages, vec![Message::new(Role::User, "hi")]);
    let written = round_trip(&json!({ "model": "m", "input": "hi" }));
    assert_eq!(
        written["input"],
        json!([{ "role": "user", "content": "hi" }])
    );
}

#[test]
fn function_calls_and_their_outputs_group_into_assistant_and_user_messages() {
    let v = json!({
        "model": "m",
        "input": [
            { "role": "user", "content": "weather?" },
            { "type": "message", "id": "msg_1", "status": "completed", "role": "assistant",
              "content": [{ "type": "output_text", "text": "let me look", "annotations": [] }] },
            { "type": "function_call", "id": "fc_1", "call_id": "c1", "name": "a", "arguments": "{}", "status": "completed" },
            { "type": "function_call", "id": "fc_2", "call_id": "c2", "name": "b", "arguments": "{\"x\":1}" },
            { "type": "function_call_output", "call_id": "c1", "output": "sunny" },
            { "type": "function_call_output", "call_id": "c2", "output": "windy" },
        ],
    });
    let chat = decode_request(&v);
    let roles: Vec<_> = chat.messages.iter().map(|m| m.role.clone()).collect();
    assert_eq!(roles, [Role::User, Role::Assistant, Role::User]);
    let assistant = &chat.messages[1];
    assert_eq!(assistant.content.len(), 3);
    assert!(
        matches!(&assistant.content[1], Content::ToolCall { id, name, .. } if id == "c1" && name == "a")
    );
    let results = &chat.messages[2];
    assert_eq!(results.content.len(), 2);
    assert!(
        matches!(&results.content[0], Content::ToolResult { id, is_error: false, .. } if id == "c1")
    );
    assert_eq!(round_trip(&v), v);
}

#[test]
fn an_item_the_kind_has_no_form_for_stays_whole() {
    let v = json!({
        "model": "m",
        "input": [
            { "role": "user", "content": "hi" },
            { "type": "item_reference", "id": "msg_9" },
            { "type": "web_search_call", "id": "ws_1", "status": "completed" },
        ],
    });
    let chat = decode_request(&v);
    assert!(matches!(&chat.messages[1].role, Role::Native { name, .. } if name == "item"));
    assert_eq!(round_trip(&v), v);
    let refused = AnthropicMessages
        .encode_request(&chat)
        .expect_err("an item");
    assert!(refused.message().contains("role"), "{}", refused.message());
}

#[test]
fn media_and_reasoning_round_trip() {
    let v = json!({
        "model": "m",
        "input": [
            { "role": "user", "content": [
                { "type": "input_image", "image_url": "data:image/png;base64,AAAA", "detail": "high" },
                { "type": "input_file", "file_data": "data:application/pdf;base64,AAAA", "filename": "a.pdf" },
                { "type": "input_file", "file_url": "https://x.test/a.pdf" },
            ] },
            { "type": "reasoning", "id": "rs_1", "summary": [{ "type": "summary_text", "text": "thought" }], "encrypted_content": "gAAA" },
            { "type": "message", "id": "msg_1", "status": "completed", "role": "assistant",
              "content": [{ "type": "output_text", "text": "hi", "annotations": [] }] },
        ],
    });
    let chat = decode_request(&v);
    assert!(
        matches!(&chat.messages[0].content[0], Content::Image(m) if m.mime_type == "image/png")
    );
    assert!(
        matches!(&chat.messages[0].content[1], Content::Document(m) if m.name.as_deref() == Some("a.pdf"))
    );
    assert!(
        matches!(&chat.messages[1].content[0], Content::Reasoning { text, .. } if text == "thought")
    );
    assert_eq!(round_trip(&v), v);
}

#[test]
fn what_the_kind_has_no_form_for_is_native_and_refused_in_translation() {
    for (field, value) in [
        ("previous_response_id", json!("resp_1")),
        ("conversation", json!("conv_1")),
        ("include", json!(["reasoning.encrypted_content"])),
        ("truncation", json!("auto")),
        ("reasoning", json!({ "effort": "xhigh" })),
        ("metadata", json!({ "k": "v" })),
        ("tools", json!([{ "type": "web_search_preview" }])),
        ("tool_choice", json!({ "type": "web_search_preview" })),
    ] {
        let mut v = json!({ "model": "m", "input": [{ "role": "user", "content": "hi" }] });
        v[field] = value.clone();
        let chat = decode_request(&v);
        assert_eq!(chat.native.fields().get(field), Some(&value), "{field}");
        assert_eq!(round_trip(&v), v, "{field}");
        for refused in [
            AnthropicMessages.encode_request(&chat),
            OpenAiChat.encode_request(&chat),
        ] {
            let message = refused.expect_err(field).message().to_owned();
            assert!(message.contains(field), "{field}: {message}");
        }
    }
}

#[test]
fn not_storing_is_sent_to_openai_and_said_to_no_other_provider() {
    let v = json!({ "model": "m", "input": [{ "role": "user", "content": "hi" }], "store": false });
    assert_eq!(round_trip(&v), v);
    let chat = decode_request(&v);
    for other in [
        AnthropicMessages.encode_request(&chat),
        OpenAiChat.encode_request(&chat),
    ] {
        let other = other.expect("translated");
        assert!(
            !other.contains_key("store") && !other.contains_key("inert"),
            "{other:?}"
        );
    }
}

#[test]
fn the_defaults_an_sdk_sends_say_nothing() {
    let v = json!({
        "model": "m", "input": "hi", "store": true, "background": false,
        "tools": [], "metadata": {}, "include": [], "stream": false,
        "text": { "format": { "type": "text" } },
    });
    let chat = decode_request(&v);
    assert!(chat.native.is_none(), "{:?}", chat.native);
    let other = AnthropicMessages.encode_request(&chat);
    assert!(other.is_ok(), "{other:?}");
}

#[test]
fn text_beyond_the_format_stays_native_and_is_written_with_the_requirement() {
    let v = json!({
        "model": "m", "input": [{ "role": "user", "content": "hi" }],
        "text": { "format": { "type": "json_object" }, "verbosity": "low" },
    });
    let chat = decode_request(&v);
    assert_eq!(
        chat.options.response_format,
        Some(ResponseFormat::JsonObject)
    );
    assert_eq!(
        chat.native.fields().get("text"),
        Some(&json!({ "verbosity": "low" }))
    );
    assert_eq!(round_trip(&v), v);
}

#[test]
fn stop_sequences_cannot_be_written() {
    let mut chat = Input::new([Message::new(Role::User, "hi")]);
    chat.options.stop = vec!["x".into()];
    let refused = OpenAiResponses.encode_request(&chat).expect_err("stop");
    assert!(refused.message().contains("stop sequences"));
}

#[test]
fn another_formats_conversation_is_written_as_responses() {
    let chat = {
        let mut chat = Input::new([
            Message::new(Role::System, "be brief"),
            Message::new(Role::User, "weather?"),
        ]);
        let mut calls = Message::new(Role::Assistant, "checking");
        calls
            .content
            .push(Content::tool_call("c1", "get", "{\"a\":1}"));
        chat.messages.push(calls);
        let mut results = Message::new(Role::User, "");
        results.content = vec![Content::tool_result("c1", vec!["rain".into()], false)];
        chat.messages.push(results);
        let mut tool = Tool::new("get", json!({ "type": "object" }));
        tool.description = Some("gets".into());
        chat.tools = vec![tool];
        chat.model = Some("m".into());
        chat.options.tool_choice = Some(ToolChoice::Required);
        chat
    };
    let v = Value::Object(OpenAiResponses.encode_request(&chat).expect("a request"));
    assert_eq!(
        v,
        json!({
            "model": "m",
            "instructions": "be brief",
            "input": [
                { "role": "user", "content": "weather?" },
                { "role": "assistant", "content": "checking" },
                { "type": "function_call", "call_id": "c1", "name": "get", "arguments": "{\"a\":1}" },
                { "type": "function_call_output", "call_id": "c1", "output": "rain" },
            ],
            "tools": [{ "type": "function", "name": "get", "description": "gets",
                        "parameters": { "type": "object" }, "strict": false }],
            "tool_choice": "required",
        })
    );
}

#[test]
fn a_failed_tool_result_and_audio_cannot_be_written() {
    let mut results = Message::new(Role::User, "");
    results.content = vec![Content::tool_result("c", vec!["boom".into()], true)];
    let chat = Input::new([results]);
    assert!(OpenAiResponses.encode_request(&chat).is_err());
}

#[test]
fn a_native_role_of_another_format_is_refused() {
    let chat = Input::new([Message::new(Role::native("openai-chat", "tool"), "x")]);
    assert!(OpenAiResponses.encode_request(&chat).is_err());
}

#[test]
fn item_ids_are_dropped_by_other_formats() {
    let v = json!({
        "model": "m",
        "input": [
            { "role": "user", "content": "hi" },
            { "type": "message", "id": "msg_1", "status": "completed", "role": "assistant",
              "content": [{ "type": "output_text", "text": "hello", "annotations": [] }] },
            { "type": "function_call", "id": "fc_1", "call_id": "c1", "name": "a", "arguments": "{}" },
            { "type": "function_call_output", "id": "fo_1", "call_id": "c1", "output": "ok" },
        ],
        "tools": [{ "type": "function", "name": "a", "parameters": { "type": "object" } }],
    });
    let chat = decode_request(&v);
    let claude = AnthropicMessages.encode_request(&chat).expect("translated");
    assert_eq!(claude["messages"][1]["content"][0]["text"], "hello");
    assert_eq!(claude["messages"][1]["content"][1]["id"], "c1");
    assert!(claude["tools"][0].get("strict").is_none());
    let gpt = OpenAiChat.encode_request(&chat).expect("translated");
    assert_eq!(gpt["messages"][1]["tool_calls"][0]["id"], "c1");
}

// ----------------------------------------------------------------- response

#[test]
fn a_stream_decodes_to_the_kinds_events() {
    let (out, end) = decode_stream(&text_stream());
    end.expect("a whole stream");
    let Output::Start(start) = &out[0] else {
        panic!("{out:?}");
    };
    assert_eq!(
        (start.id.as_str(), start.model.as_str()),
        ("resp_1", "gpt-x")
    );
    assert_eq!(start.native.fields().get("created_at"), Some(&json!(5)));
    let rest = [
        Output::native(
            RESPONSES,
            json!({ "type": "block_fields", "block": 0, "fields": { "id": "msg_1" } }),
        ),
        Output::block_start(0, Block::Text),
        Output::text(0, "Hello"),
        Output::text(0, " world"),
        Output::block_end(0),
    ];
    assert_eq!(out[1..6], rest);
    let mut usage = Usage::new(5, 3);
    usage.cached_input_tokens = 2;
    usage.reasoning_tokens = 1;
    assert_eq!(
        out[6..],
        [Output::Usage(usage), Output::finished(Finish::Stop)]
    );
}

#[test]
fn a_message_streamed_without_its_parts_events_decodes_alike() {
    let part = |e: &Value| {
        e["type"]
            .as_str()
            .is_some_and(|t| t.starts_with("response.content_part."))
    };
    let unparted: Vec<_> = text_stream().into_iter().filter(|e| !part(e)).collect();
    let (out, end) = decode_stream(&unparted);
    end.expect("a whole stream");
    assert_eq!(out, decode_stream(&text_stream()).0);
}

#[test]
fn a_whole_body_decodes_to_the_same_events() {
    let done = text_stream().pop().expect("completed");
    let (streamed, _) = decode_stream(&text_stream());
    let whole = decode_body(&done["response"]);
    // A body says the text at once.
    let joined = |events: &[Output]| {
        let mut all = Assembled::default();
        events.iter().for_each(|e| all.push(e).expect("events"));
        (all.text(), all.usage, all.finish, all.start.map(|s| s.id))
    };
    assert_eq!(joined(&whole), joined(&streamed));
}

#[test]
fn function_calls_and_reasoning_are_blocks() {
    let stream = [
        json!({ "type": "response.created", "response": response("in_progress", json!([])) }),
        json!({ "type": "response.output_item.added", "output_index": 0, "item": { "id": "rs_1", "type": "reasoning", "summary": [] } }),
        json!({ "type": "response.reasoning_summary_part.added", "item_id": "rs_1", "output_index": 0, "summary_index": 0, "part": { "type": "summary_text", "text": "" } }),
        json!({ "type": "response.reasoning_summary_text.delta", "item_id": "rs_1", "output_index": 0, "summary_index": 0, "delta": "think" }),
        json!({ "type": "response.reasoning_summary_text.done", "item_id": "rs_1", "output_index": 0, "summary_index": 0, "text": "think" }),
        json!({ "type": "response.reasoning_summary_part.added", "item_id": "rs_1", "output_index": 0, "summary_index": 1, "part": { "type": "summary_text", "text": "" } }),
        json!({ "type": "response.reasoning_summary_text.delta", "item_id": "rs_1", "output_index": 0, "summary_index": 1, "delta": "more" }),
        json!({ "type": "response.output_item.done", "output_index": 0, "item": { "id": "rs_1", "type": "reasoning", "summary": [{ "type": "summary_text", "text": "think" }, { "type": "summary_text", "text": "more" }], "encrypted_content": "gAAA" } }),
        json!({ "type": "response.output_item.added", "output_index": 1, "item": { "id": "fc_1", "type": "function_call", "status": "in_progress", "arguments": "", "call_id": "c1", "name": "get" } }),
        json!({ "type": "response.function_call_arguments.delta", "item_id": "fc_1", "output_index": 1, "delta": "{\"a\"" }),
        json!({ "type": "response.function_call_arguments.delta", "item_id": "fc_1", "output_index": 1, "delta": ":1}" }),
        json!({ "type": "response.function_call_arguments.done", "item_id": "fc_1", "output_index": 1, "arguments": "{\"a\":1}" }),
        json!({ "type": "response.output_item.done", "output_index": 1, "item": { "id": "fc_1", "type": "function_call", "status": "completed", "arguments": "{\"a\":1}", "call_id": "c1", "name": "get" } }),
        json!({ "type": "response.completed", "response": response("completed", json!([])) }),
    ];
    let (out, end) = decode_stream(&stream);
    end.expect("a stream");
    let mut all = Assembled::default();
    out.iter().for_each(|e| all.push(e).expect("events"));
    let blocks: Vec<_> = all
        .blocks()
        .map(|b| (b.kind.clone(), b.text.clone()))
        .collect();
    assert_eq!(
        blocks,
        [
            (Block::Reasoning, "think\n\nmore".to_owned()),
            (call("c1", "get"), "{\"a\":1}".to_owned()),
        ]
    );
    assert_eq!(all.finish, Some(Finish::ToolUse));
    assert!(out.contains(&Output::native(
        RESPONSES,
        json!({ "type": "block_fields", "block": 0, "fields": { "id": "rs_1" } })
    )));
}

#[test]
fn a_body_with_a_call_and_reasoning_decodes_alike() {
    let body = response(
        "completed",
        json!([
            { "id": "rs_1", "type": "reasoning", "summary": [{ "type": "summary_text", "text": "think" }] },
            { "id": "fc_1", "type": "function_call", "status": "completed", "arguments": "{\"a\":1}", "call_id": "c1", "name": "get" },
        ]),
    );
    let out = decode_body(&body);
    assert!(out.contains(&Output::reasoning(0, "think")));
    assert!(out.contains(&Output::arguments(1, "{\"a\":1}")));
    assert_eq!(out.last(), Some(&Output::finished(Finish::ToolUse)));
}

#[test]
fn incomplete_has_its_reason_as_the_finish() {
    for (reason, finish) in [
        ("max_output_tokens", Finish::Length),
        ("content_filter", Finish::Filtered),
        ("other", Finish::native(RESPONSES, "other")),
    ] {
        let mut body = response("incomplete", json!([]));
        body["incomplete_details"] = json!({ "reason": reason });
        let out = decode_body(&body);
        assert_eq!(
            out.last(),
            Some(&Output::finished(finish.clone())),
            "{reason}"
        );
        let written = body_of(&out).expect("a body");
        assert_eq!(written["status"], "incomplete");
        assert_eq!(written["incomplete_details"]["reason"], reason);
    }
}

#[test]
fn a_failed_response_and_an_error_event_are_failures() {
    let failed = json!({ "type": "response.failed", "response": {
        "id": "resp_1", "status": "failed", "error": { "code": "server_error", "message": "boom" } } });
    let (_, end) = decode_stream(&[failed]);
    let f = end.expect_err("failed");
    assert_eq!(f.message(), "boom");
    assert_eq!(f.code(), "server_error");

    let error = json!({ "type": "error", "code": "rate_limit_exceeded", "message": "slow", "param": null, "sequence_number": 3 });
    let (_, end) = decode_stream(&[error]);
    let f = end.expect_err("error");
    assert_eq!(f.message(), "slow");

    let mut body = response("failed", json!([]));
    body["error"] = json!({ "code": "server_error", "message": "boom" });
    let f = ResponsesDecoder::default()
        .body(&body, &mut Vec::new())
        .expect_err("failed body");
    assert_eq!(f.message(), "boom");
}

#[test]
fn a_stream_cut_before_its_end_is_truncated() {
    let mut events = text_stream();
    events.pop();
    let (_, end) = decode_stream(&events);
    assert!(end.is_err());
}

#[test]
fn items_the_kind_has_no_form_for_are_native_and_dropped_in_translation() {
    let item = json!({ "id": "ws_1", "type": "web_search_call", "status": "completed" });
    let stream = [
        json!({ "type": "response.created", "response": response("in_progress", json!([])) }),
        json!({ "type": "response.output_item.added", "output_index": 0, "item": item }),
        json!({ "type": "response.web_search_call.searching", "output_index": 0, "item_id": "ws_1" }),
        json!({ "type": "response.output_item.done", "output_index": 0, "item": item }),
        json!({ "type": "response.completed", "response": response("completed", json!([item])) }),
    ];
    let (out, end) = decode_stream(&stream);
    end.expect("a stream");
    let natives = out
        .iter()
        .filter(|e| matches!(e, Output::Native(..)))
        .count();
    assert_eq!(natives, 3);
    // Written again in this format, the item and its events are as they came.
    let written = stream_of(&out).expect("a stream");
    assert_eq!(
        types(&written),
        [
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.web_search_call.searching",
            "response.output_item.done",
            "response.completed",
        ]
    );
    assert_eq!(written[5]["response"]["output"], json!([item]));
    let sequence: Vec<_> = written
        .iter()
        .map(|e| e["sequence_number"].clone())
        .collect();
    assert_eq!(sequence, (0..6).map(|n| json!(n)).collect::<Vec<_>>());
    // Another format drops it: only a client of this one could read it.
    let mut whole = Assembled::default();
    out.iter().for_each(|e| whole.push(e).expect("events"));
    for body in [
        AnthropicMessages.encode_whole(&whole),
        OpenAiChat.encode_whole(&whole),
    ] {
        let body = body.expect("a body");
        assert!(!body.to_string().contains("web_search_call"), "{body}");
    }
    // A body writes it in its output.
    assert_eq!(body_of(&out).expect("a body")["output"], json!([item]));
}

#[test]
fn a_stream_is_written_as_the_documented_sequence() {
    let mut usage = Usage::new(5, 3);
    usage.cached_input_tokens = 2;
    usage.reasoning_tokens = 1;
    let replies = [
        Output::start("chatcmpl-1", "gpt-x"),
        Output::block_start(7, Block::Reasoning),
        Output::reasoning(7, "think"),
        Output::block_end(7),
        Output::block_start(8, Block::Text),
        Output::text(8, "Hel"),
        Output::text(8, "lo"),
        Output::block_end(8),
        Output::block_start(9, call("c1", "get")),
        Output::arguments(9, "{\"a\":1}"),
        Output::block_end(9),
        Output::Usage(usage),
        Output::finished(Finish::ToolUse),
    ];
    let events = stream_of(&replies).expect("a stream");
    assert_eq!(
        types(&events),
        [
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.reasoning_summary_part.added",
            "response.reasoning_summary_text.delta",
            "response.reasoning_summary_text.done",
            "response.reasoning_summary_part.done",
            "response.output_item.done",
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.delta",
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
            "response.output_item.added",
            "response.function_call_arguments.delta",
            "response.function_call_arguments.done",
            "response.output_item.done",
            "response.completed",
        ]
    );
    let sequence: Vec<_> = events
        .iter()
        .map(|e| e["sequence_number"].as_u64())
        .collect();
    assert_eq!(sequence, (0..20).map(Some).collect::<Vec<_>>());
    let indexes: Vec<_> = events
        .iter()
        .filter_map(|e| e["output_index"].as_u64())
        .collect();
    assert_eq!(indexes, [0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2]);
    let created = &events[0]["response"];
    assert_eq!(created["status"], "in_progress");
    assert_eq!(created["output"], json!([]));
    assert_eq!(created["id"], "chatcmpl-1");
    // The items name themselves, and the deltas name their item.
    let message_id = events[2 + 6]["item"]["id"].clone();
    assert!(message_id.as_str().is_some_and(|id| id.starts_with("msg_")));
    assert_eq!(events[10]["item_id"], message_id);
    assert_eq!(events[10]["content_index"], 0);
    assert_eq!(events[12]["text"], "Hello");
    assert_eq!(events[17]["arguments"], "{\"a\":1}");
    assert_eq!(events[17]["name"], "get");
    // The terminal event has the response whole.
    let done = &events[19]["response"];
    assert_eq!(done["status"], "completed");
    assert_eq!(done["output"].as_array().map(Vec::len), Some(3));
    assert_eq!(
        done["output"][0]["summary"],
        json!([{ "type": "summary_text", "text": "think" }])
    );
    assert_eq!(done["output"][1]["content"][0]["text"], "Hello");
    assert_eq!(done["output"][2]["call_id"], "c1");
    assert_eq!(done["usage"]["input_tokens"], 5);
    assert_eq!(done["usage"]["input_tokens_details"]["cached_tokens"], 2);
    assert_eq!(
        done["usage"]["output_tokens_details"]["reasoning_tokens"],
        1
    );
    assert_eq!(done["usage"]["total_tokens"], 8);
    assert!(done["completed_at"].is_u64());
}

#[test]
fn blocks_that_interleave_are_written_one_at_a_time() {
    let replies = [
        Output::start("r", "m"),
        Output::block_start(0, call("c1", "a")),
        Output::block_start(1, call("c2", "b")),
        Output::arguments(0, "{}"),
        Output::arguments(1, "{\"x\":1}"),
        Output::block_end(0),
        Output::block_end(1),
        Output::finished(Finish::ToolUse),
    ];
    let events = stream_of(&replies).expect("a stream");
    let calls: Vec<_> = events
        .iter()
        .filter(|e| e["type"] == "response.output_item.done")
        .map(|e| (e["output_index"].clone(), e["item"]["call_id"].clone()))
        .collect();
    assert_eq!(calls, [(json!(0), json!("c1")), (json!(1), json!("c2"))]);
    let order: Vec<_> = events
        .iter()
        .filter(|e| e["type"].as_str().is_some_and(|t| t.contains("item")))
        .map(|e| {
            (
                e["type"].as_str().unwrap_or("").to_owned(),
                e["output_index"].as_u64(),
            )
        })
        .collect();
    assert_eq!(
        order[..2],
        [
            ("response.output_item.added".to_owned(), Some(0)),
            ("response.output_item.done".to_owned(), Some(0))
        ]
    );
}

#[test]
fn a_body_is_written_from_the_events() {
    let replies = [
        Output::start("resp_9", "gpt-x"),
        Output::native(
            RESPONSES,
            json!({ "type": "block_fields", "block": 0, "fields": { "id": "msg_real" } }),
        ),
        Output::block_start(0, Block::Text),
        Output::text(0, "Hi"),
        Output::block_end(0),
        Output::Usage(Usage::new(4, 1)),
        Output::finished(Finish::Length),
    ];
    let body = body_of(&replies).expect("a body");
    assert_eq!(body["object"], "response");
    assert_eq!(body["id"], "resp_9");
    assert_eq!(body["status"], "incomplete");
    assert_eq!(
        body["incomplete_details"],
        json!({ "reason": "max_output_tokens" })
    );
    assert_eq!(body["output"][0]["id"], "msg_real");
    assert_eq!(
        body["output"][0]["content"],
        json!([{ "type": "output_text", "text": "Hi", "annotations": [] }])
    );
    assert_eq!(body["usage"]["total_tokens"], 5);
    for key in [
        "instructions",
        "metadata",
        "parallel_tool_calls",
        "tool_choice",
        "tools",
        "error",
        "created_at",
    ] {
        assert!(body.get(key).is_some(), "{key} is part of a response");
    }
}

#[test]
fn a_provider_s_own_fields_come_back_in_this_format() {
    let mut start = Start::new("resp_1", "gpt-x");
    start.native = native_fields(
        RESPONSES,
        json!({ "created_at": 77, "temperature": 0.2, "service_tier": "default" })
            .as_object()
            .cloned()
            .unwrap_or_default(),
    );
    let replies = [Output::Start(start), Output::finished(Finish::Stop)];
    let body = body_of(&replies).expect("a body");
    assert_eq!(body["created_at"], 77);
    assert_eq!(body["temperature"], 0.2);
    assert_eq!(body["service_tier"], "default");
    // Another format's fields are not ours.
    let mut foreign = Start::new("r", "m");
    foreign.native = Some(NativeFields::new("openai-chat", {
        let mut m = Map::new();
        m.insert("system_fingerprint".into(), "fp".into());
        m
    }));
    let body = body_of(&[Output::Start(foreign), Output::finished(Finish::Stop)]).expect("a body");
    assert!(body.get("system_fingerprint").is_none());
}

#[test]
fn usage_that_cannot_be_written_is_refused() {
    let mut usage = Usage::new(1, 1);
    usage.cached_input_tokens = 2;
    assert!(body_of(&[Output::Usage(usage)]).is_err());
    let mut usage = Usage::new(5, 1);
    usage.reasoning_tokens = 2;
    assert!(stream_of(&[Output::Usage(usage)]).is_err());
    // A cache write is a field of the input's details.
    let mut usage = Usage::new(5, 1);
    usage.cache_write_input_tokens = 3;
    let body = body_of(&[Output::Usage(usage)]).expect("a body");
    assert_eq!(
        body["usage"]["input_tokens_details"]["cache_write_tokens"],
        3
    );
}

#[test]
fn a_text_that_no_one_wrote_is_still_an_item() {
    let replies = [
        Output::block_start(0, Block::Text),
        Output::block_end(0),
        Output::finished(Finish::Stop),
    ];
    let events = stream_of(&replies).expect("a stream");
    assert!(types(&events).contains(&"response.content_part.done"));
    let body = body_of(&replies).expect("a body");
    assert_eq!(body["output"][0]["content"][0]["text"], "");
}

#[test]
fn another_formats_events_are_dropped() {
    let ping = Output::native("anthropic-messages", json!({ "type": "ping" }));
    let stray = Output::native("openai-chat", json!({ "choices": [{ "index": 1 }] }));
    let replies = [
        Output::start("r", "m"),
        ping,
        stray,
        Output::finished(Finish::Stop),
    ];
    let stream = stream_of(&replies).expect("a stream");
    let written = serde_json::to_string(&stream).expect("json");
    assert!(
        !written.contains("ping") && !written.contains("choices"),
        "{written}"
    );
    let body = body_of(&replies).expect("a body");
    assert!(!body.to_string().contains("choices"), "{body}");
}

#[test]
fn an_error_after_content_ends_the_stream_with_an_error_event() {
    let mut enc = encode::Events::default();
    let mut out = BytesMut::new();
    for reply in [
        Output::start("r", "m"),
        Output::block_start(0, Block::Text),
        Output::text(0, "Hi"),
    ] {
        enc.encode_into(&reply, &mut out).expect("events");
    }
    enc.end_into(
        &Err(Fault::new("lost").with(respan_gateway_core::ErrorCode("upstream".into()))),
        &mut out,
    )
    .expect("ends");
    let text = String::from_utf8_lossy(&out).into_owned();
    let last = text.trim_end().rsplit("\n\n").next().unwrap_or_default();
    let error = event_json(last.as_bytes()).expect("an error event");
    assert_eq!(error["type"], "error");
    assert_eq!(error["message"], "lost");
    assert!(text.contains("event: error\n"));
}

#[test]
fn errors_are_written_as_an_error_object() {
    let body = OpenAiResponses::error_body(&Fault::new("nope"));
    assert_eq!(body["error"]["message"], "nope");
    assert!(body["error"].get("code").is_some());
}

#[test]
fn a_response_is_recognized_by_its_object() {
    assert!(OpenAiResponses::is_response(
        &json!({ "object": "response" })
    ));
    assert!(!OpenAiResponses::is_response(&json!({ "choices": [] })));
}

#[test]
fn a_chat_tools_strict_is_written_as_responses_own() {
    let body = json!({
        "model": "m",
        "messages": [{ "role": "user", "content": "hi" }],
        "tools": [
            { "type": "function", "function": { "name": "a", "strict": true, "parameters": { "type": "object" } } },
            { "type": "function", "function": { "name": "b", "parameters": { "type": "object" } } },
        ],
    });
    let chat = OpenAiChat
        .decode_request(Bytes::from(body.to_string()))
        .expect("decoded");
    let v = OpenAiResponses.encode_request(&chat).expect("translated");
    assert_eq!(v["tools"][0]["strict"], true);
    assert_eq!(
        v["tools"][1]["strict"], false,
        "Chat Completions' tools are not strict by default"
    );
}

#[test]
fn chat_fields_responses_has_too_are_written_as_its_own() {
    let body = json!({
        "model": "m", "messages": [{ "role": "user", "content": "hi" }],
        "store": false, "metadata": { "a": "b" }, "service_tier": "flex",
        "prompt_cache_key": "k", "verbosity": "low",
    });
    let chat = OpenAiChat
        .decode_request(Bytes::from(body.to_string()))
        .expect("decoded");
    let v = OpenAiResponses.encode_request(&chat).expect("translated");
    // Whether OpenAI keeps a chat is Chat Completions' own: not carried.
    assert!(v.get("store").is_none());
    assert_eq!(v["metadata"], json!({ "a": "b" }));
    assert_eq!(v["service_tier"], "flex");
    assert_eq!(v["prompt_cache_key"], "k");
    assert_eq!(v["text"]["verbosity"], "low");
    // One Responses lacks is still refused.
    let n = json!({ "model": "m", "messages": [{ "role": "user", "content": "hi" }], "n": 2 });
    let chat = OpenAiChat
        .decode_request(Bytes::from(n.to_string()))
        .expect("decoded");
    assert!(OpenAiResponses.encode_request(&chat).is_err());
}

#[test]
fn instructions_count_as_the_reader_reads_them() {
    for (body, instructed) in [
        (r#"{"instructions":"Be brief."}"#, true),
        (r#"{"instructions":""}"#, true),
        (
            r#"{"instructions":[{"type":"input_text","text":"x"}]}"#,
            true,
        ),
        (r#"{"instructions":[]}"#, false),
        (r#"{"instructions":null}"#, false),
        (r#"{"input":"hi"}"#, false),
        ("not json", false),
    ] {
        assert_eq!(
            OpenAiResponses::instructed(body.as_bytes()),
            instructed,
            "{body}"
        );
    }
}

#[test]
fn a_foreign_value_is_dropped_beside_what_it_can_write() {
    let foreign = NativeValue::new("openai-chat", json!({"logprobs": {}, "refusal": "no"}));
    let replies = [
        Output::start("r1", "m"),
        Output::block_start(0, Block::Text),
        Output::text(0, "hi"),
        Output::block_end(0),
        Output::Native(foreign),
    ];
    let body = body_of(&replies).expect("a body");
    assert_eq!(body["output"][0]["content"][0]["text"], "hi");
    assert!(!body.to_string().contains("refusal"), "{body}");
    let stream = stream_of(&replies).expect("a stream");
    let written = serde_json::to_string(&stream).expect("json");
    assert!(
        written.contains("\"hi\"") && !written.contains("refusal"),
        "{written}"
    );
}
