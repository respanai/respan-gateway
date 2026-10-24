use super::*;

use respan_gateway_http::bytes::{Bytes, BytesMut};

use respan_gateway_conversation::ToolChoice;
use respan_gateway_core::NativeFields;
use respan_gateway_http::http::{HeaderValue, header::CONTENT_TYPE};

/// The events of `events`, each decoded in turn.
fn decode(events: &[&str]) -> Vec<Output> {
    let mut reader = AnthropicDecoder::default();
    let mut out = Vec::new();
    for e in events {
        let _ = reader.event(Bytes::from(e.to_string()), &mut out);
    }
    out
}

/// The events of a whole body.
fn decode_body(v: &Value) -> Vec<Output> {
    let mut out = Vec::new();
    AnthropicDecoder::default()
        .body(v, &mut out)
        .expect("a whole response");
    out
}

/// `replies` as a stream, whole.
fn stream_of(replies: &[Output]) -> Result<String, Fault> {
    let mut enc = EventStream::default();
    let mut out = BytesMut::new();
    for reply in replies {
        enc.encode_into(reply, &mut out)?;
    }
    enc.end_into(&Ok(()), &mut out)?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// The JSON of each event of a stream.
fn stream_events(stream: &str) -> Vec<Value> {
    let data = stream
        .split("\n\n")
        .filter_map(|e| e.lines().find_map(|l| l.strip_prefix("data: ")));
    data.filter_map(|d| serde_json::from_str(d).ok()).collect()
}

/// `replies` as a whole body.
fn body_of(replies: &[Output]) -> Result<Value, Fault> {
    let mut whole = Assembled::default();
    replies.iter().try_for_each(|r| whole.push(r))?;
    AnthropicMessages.encode_whole(&whole)
}

fn call(id: &str, name: &str) -> Block {
    Block::ToolCall {
        id: id.into(),
        name: name.into(),
    }
}

/// A block is any number unique in the response, such as a plugin's
/// own: each takes the next index as it opens on the wire.
#[test]
fn blocks_of_any_numbers_take_indexes_in_the_order_they_open() {
    let far = u32::MAX - 1;
    let replies = [
        Output::start("x", "m"),
        Output::block_start(far, Block::Reasoning),
        Output::reasoning(far, "hm"),
        Output::block_end(far),
        Output::block_start(7, Block::Text),
        Output::text(7, "hi"),
        Output::block_end(7),
        Output::block_start(u32::MAX, call("t", "f")),
        Output::block_end(u32::MAX),
    ];
    let stream = stream_of(&replies).expect("a stream");
    let starts: Vec<Value> = stream_events(&stream)
        .into_iter()
        .filter(|e| e["type"] == "content_block_start")
        .map(|e| json!([e["index"], e["content_block"]["type"]]))
        .collect();
    let expected = json!([[0, "thinking"], [1, "text"], [2, "tool_use"]]);
    assert_eq!(Value::from(starts), expected);
    let body = body_of(&replies).expect("a body");
    let kinds: Vec<&Value> = body["content"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|b| &b["type"])
        .collect();
    assert_eq!(kinds, ["thinking", "text", "tool_use"]);
}

fn text_events(block: u32, text: &str) -> Vec<Output> {
    vec![
        Output::block_start(block, Block::Text),
        Output::text(block, text),
        Output::block_end(block),
    ]
}

/// The client's `body` as the conversation it says, with `edit` made,
/// written as a request.
fn written(body: Value, edit: impl FnOnce(&mut Input)) -> Result<Value, Fault> {
    let mut chat = AnthropicMessages.decode_request(Bytes::from(body.to_string()))?;
    edit(&mut chat);
    Ok(AnthropicMessages.encode_request(&chat)?.into())
}

#[test]
fn an_edited_tool_call_and_result_keep_their_blocks() {
    let marker = json!({ "type": "ephemeral" });
    let body = json!({ "model": "c", "max_tokens": 5,
    "tools": [{ "name": "lookup", "input_schema": { "type": "object" }, "cache_control": marker }],
    "tool_choice": { "type": "tool", "name": "lookup", "disable_parallel_tool_use": true },
    "messages": [
        { "role": "user", "content": "find bob" },
        { "role": "assistant", "content": [
            { "type": "tool_use", "id": "t1", "name": "lookup", "input": { "email": "bob@x.io" } },
        ] },
        { "role": "user", "content": [
            { "type": "tool_result", "tool_use_id": "t1", "content": "bob@x.io is Bob" },
        ] },
    ]});
    let chat = AnthropicMessages.decode_json(&body);
    assert_eq!(
        chat.tools[0].native.fields().get("cache_control"),
        Some(&marker)
    );
    assert_eq!(chat.tools[0].native.format(), Some(ANTHROPIC));
    assert_eq!(
        chat.options.tool_choice,
        Some(ToolChoice::Tool("lookup".into()))
    );
    assert_eq!(chat.options.parallel_tool_calls, Some(false));
    assert_eq!(
        chat.messages[2].content,
        [Content::tool_result(
            "t1",
            vec!["bob@x.io is Bob".into()],
            false
        )]
    );
    assert_eq!(written(body.clone(), |_| {}).ok(), Some(body.clone()));
    let edited = written(body.clone(), |c| {
        for part in c.messages.iter_mut().flat_map(|m| &mut m.content) {
            match part {
                Content::ToolCall { arguments, .. } => {
                    *arguments = arguments.replace("bob@x.io", "<EMAIL_1>");
                }
                Content::ToolResult { content, .. } => {
                    *content = vec!["<EMAIL_1> is Bob".into()];
                }
                _ => {}
            }
        }
    });
    let mut expected = body;
    expected["messages"][1]["content"][0]["input"] = json!({ "email": "<EMAIL_1>" });
    expected["messages"][2]["content"][0]["content"] = "<EMAIL_1> is Bob".into();
    assert_eq!(edited.ok(), Some(expected));
}

#[test]
fn a_request_round_trips_through_a_conversation() {
    let body = json!({
        "model": "claude",
        "max_tokens": 10,
        "system": [{ "type": "text", "text": "be brief" }],
        "messages": [{ "role": "user", "content": "hi" }],
        "stop_sequences": ["x"],
    });
    assert_eq!(
        written(body, |_| {}).ok(),
        Some(json!({
            "model": "claude",
            "max_tokens": 10,
            "system": "be brief",
            "messages": [{ "role": "user", "content": "hi" }],
            "stop_sequences": ["x"],
        }))
    );
}

/// The Messages request an OpenAI request `extra` would be, of `format`:
/// `chat` or `responses`.
fn messages_of(format: &str, extra: Value) -> Result<Map<String, Value>, Fault> {
    let mut body = match format {
        "chat" => json!({ "model": "claude", "messages": [{ "role": "user", "content": "hi" }] }),
        _ => json!({ "model": "claude", "input": "hi" }),
    };
    with_fields(&mut body, extra);
    let raw = Bytes::from(body.to_string());
    let chat = match format {
        "chat" => crate::openai::OpenAiChat.decode_request(raw),
        _ => crate::responses::OpenAiResponses.decode_request(raw),
    };
    chat.and_then(|chat| AnthropicMessages.encode_request(&chat))
}

fn with_fields(body: &mut Value, extra: Value) {
    if let (Some(body), Value::Object(extra)) = (body.as_object_mut(), extra) {
        body.extend(extra);
    }
}

#[test]
fn an_openai_requests_messages_fields_are_written_as_messages_says_them() {
    let cases = [
        (
            json!({ "inference_geo": "us" }),
            "/inference_geo",
            json!("us"),
        ),
        (
            json!({ "service_tier": "standard_only" }),
            "/service_tier",
            json!("standard_only"),
        ),
        (json!({ "speed": "fast" }), "/speed", json!("fast")),
        (json!({ "top_k": 5 }), "/top_k", json!(5)),
        (
            json!({ "cache_control": { "type": "ephemeral" } }),
            "/cache_control",
            json!({ "type": "ephemeral" }),
        ),
        (
            json!({ "safety_identifier": "u-1" }),
            "/metadata/user_id",
            json!("u-1"),
        ),
        (
            json!({ "safety_identifier": "u-1", "user": "u-2" }),
            "/metadata/user_id",
            json!("u-2"),
        ),
    ];
    for format in ["chat", "responses"] {
        for (extra, at, value) in &cases {
            let written = messages_of(format, extra.clone());
            let written = written.map(|v| Value::Object(v).pointer(at).cloned());
            assert_eq!(
                written.ok().flatten().as_ref(),
                Some(value),
                "{format} {extra}"
            );
        }
    }
}

#[test]
fn a_messages_request_for_where_or_how_it_is_served_is_refused_by_openai() {
    for extra in [
        json!({}),
        json!({ "inference_geo": "us" }),
        json!({ "service_tier": "standard_only" }),
        json!({ "speed": "fast" }),
    ] {
        let mut body = json!({ "model": "c", "max_tokens": 5,
            "messages": [{ "role": "user", "content": "hi" }] });
        with_fields(&mut body, extra.clone());
        let chat = AnthropicMessages.decode_request(Bytes::from(body.to_string()));
        let chat = chat.expect("a conversation");
        let to_chat = crate::openai::OpenAiChat.encode_request(&chat);
        let to_responses = crate::responses::OpenAiResponses.encode_request(&chat);
        for written in [to_chat, to_responses] {
            match extra.as_object().is_some_and(Map::is_empty) {
                true => assert!(written.is_ok()),
                false => {
                    let client = |f: Fault| f.is(respan_gateway_core::FaultKind::Client);
                    assert!(written.is_err_and(client), "{extra}");
                }
            }
        }
    }
}

#[test]
fn the_metadata_user_id_is_the_user() {
    let body = json!({ "model": "c", "max_tokens": 5,
        "metadata": { "user_id": "bob@x.io" },
        "messages": [{ "role": "user", "content": "hi" }] });
    let chat = AnthropicMessages.decode_json(&body);
    assert_eq!(chat.user.as_deref(), Some("bob@x.io"));
    assert_eq!(written(body.clone(), |_| {}).ok(), Some(body.clone()));
    let redacted = written(body.clone(), |c| c.user = Some("[x]".into()));
    let metadata = redacted.ok().map(|v| v["metadata"].clone());
    assert_eq!(metadata, Some(json!({ "user_id": "[x]" })));
    let removed = written(body, |c| c.user = None);
    assert_eq!(removed.ok().map(|v| v.get("metadata").cloned()), Some(None));
}

#[test]
fn a_count_written_as_a_whole_float_counts_and_any_other_is_refused() {
    let floats = usage_of(&json!({ "input_tokens": 12.0, "output_tokens": 3 }));
    let floats = floats.expect("a whole float is a count");
    assert_eq!(floats.input_tokens, 12);
    assert_eq!(floats.output_tokens, 3);
    for bad in [json!(1.5), json!(-1), json!("7")] {
        let refused = usage_of(&json!({ "input_tokens": 1, "output_tokens": bad }));
        assert!(
            refused.is_err_and(|f| f.is(respan_gateway_core::FaultKind::Provider)),
            "{bad}"
        );
    }
    let none = usage_of(&json!({ "input_tokens": null })).expect("null says none");
    assert!(none.is_empty());
}

#[test]
fn input_usage_counts_cache_reads_and_writes() {
    let u = usage_of(&json!({
        "input_tokens": 3,
        "cache_read_input_tokens": 10,
        "cache_creation_input_tokens": 2,
        "output_tokens": 1,
    }))
    .expect("whole counts");
    assert_eq!(u.input_tokens, 15);
    assert_eq!(u.cached_input_tokens, 10);
    assert_eq!(u.output_tokens, 1);
}

#[test]
fn stream_usage_is_counted_once_and_thinking_is_reasoning() {
    let out = decode(&[
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"c\",\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hm\"}}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":5,\"output_tokens\":7}}\n\n",
    ]);
    let mut total = Usage::default();
    for r in &out {
        if let Output::Usage(u) = r {
            total += u;
        }
    }
    assert_eq!(total.input_tokens, 5);
    assert_eq!(total.output_tokens, 7);
    assert!(out.contains(&Output::reasoning(0, "hm")));
}

#[test]
fn an_event_it_cannot_read_is_content_filters_see() {
    let out = decode(&[
        "event: future\ndata: {\"type\":\"future\",\"text\":\"secret\"}\n\n",
        "data: not json\n\n",
        "event: ping\ndata: {\"type\":\"ping\"}\n\n",
        ": a comment\n\n",
    ]);
    // A comment says nothing; the others are events of this format.
    assert_eq!(
        out,
        [
            Output::native(ANTHROPIC, json!({ "type": "future", "text": "secret" })),
            Output::native(ANTHROPIC, "not json"),
            Output::native(ANTHROPIC, json!({ "type": "ping" })),
        ]
    );
    assert!(out.iter().all(|r| !r.is_content()));
}

#[test]
fn text_a_block_opens_with_is_text() {
    let out = decode(&[
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"text\",\"text\":\"Well, \"}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":3,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    ]);
    assert_eq!(
        out,
        [
            Output::block_start(2, Block::Text),
            Output::text(2, "Well, "),
            // An empty text opens a block that says nothing yet.
            Output::block_start(3, Block::Text),
        ]
    );
}

#[test]
fn a_system_message_goes_in_the_system_prompt_only_before_the_conversation() {
    let mut chat = Input::new([
        Message::new(Role::System, "be brief"),
        Message::new(Role::User, "hi"),
    ]);
    let v = AnthropicMessages.encode_request(&chat).map(Value::from);
    let v = v.unwrap_or_default();
    assert_eq!(v["system"], "be brief");
    assert_eq!(v["messages"], json!([{ "role": "user", "content": "hi" }]));
    chat.messages.push(Message::new(Role::System, "later"));
    assert!(AnthropicMessages.encode_request(&chat).is_err());
}

#[test]
fn a_temperature_above_1_is_refused() {
    let mut chat = Input::new([Message::new(Role::User, "hi")]);
    chat.options.temperature = Some(1.5);
    assert!(AnthropicMessages.encode_request(&chat).is_err());
    chat.options.temperature = Some(1.0);
    assert!(AnthropicMessages.encode_request(&chat).is_ok());
}

#[test]
fn an_empty_turn_but_the_last_is_dropped() {
    let chat = Input::new([
        Message::new(Role::User, "hi"),
        Message::new(Role::Assistant, ""),
        Message::new(Role::User, "again"),
        Message::new(Role::Assistant, ""),
    ]);
    let v = AnthropicMessages.encode_request(&chat).map(Value::from);
    assert_eq!(
        v.map(|v| v["messages"].clone()).ok(),
        Some(json!([
            { "role": "user", "content": "hi" },
            { "role": "user", "content": "again" },
            { "role": "assistant", "content": "" },
        ]))
    );
}

#[test]
fn a_request_of_only_system_messages_is_refused() {
    let chat = Input::new([Message::new(Role::System, "be brief")]);
    assert!(AnthropicMessages.encode_request(&chat).is_err());
}

#[test]
fn a_system_prompt_with_cache_markers_is_written_as_it_is() {
    let system = json!([
        { "type": "text", "text": "the repo", "cache_control": { "type": "ephemeral" } },
        { "type": "text", "text": "be brief" },
    ]);
    let body = json!({ "model": "m", "max_tokens": 5, "system": system,
        "messages": [{ "role": "user", "content": "hi" }] });
    // Edited elsewhere, it is as it was.
    let edited = written(body.clone(), |c| c.messages[1].content = vec!["HI".into()]);
    assert_eq!(edited.map(|v| v["system"].clone()).ok(), Some(system));
    // A part with a marker is text, with the marker in its own native.
    let chat = AnthropicMessages.decode_json(&body);
    assert!(chat.messages[0].is_plain());
    let Content::Text { text, native, .. } = &chat.messages[0].content[0] else {
        panic!("{:?}", chat.messages[0].content);
    };
    assert_eq!(text, "the repo");
    assert_eq!(
        native.fields().get("cache_control"),
        Some(&json!({ "type": "ephemeral" }))
    );
    assert_eq!(chat.messages[0].content[1], Content::from("be brief"));
}

#[test]
fn a_system_prompt_redacted_beside_an_inserted_one_keeps_its_cache_marker() {
    let marked =
        json!({ "type": "text", "text": "key sk-1", "cache_control": { "type": "ephemeral" } });
    let body = json!({ "model": "m", "max_tokens": 5, "system": [marked],
        "messages": [{ "role": "user", "content": "hi" }] });
    let edited = written(body, |c| {
        c.messages.insert(0, Message::new(Role::System, "first"));
    });
    assert_eq!(
        edited.map(|v| v["system"].clone()).ok(),
        Some(json!([{ "type": "text", "text": "first" }, marked]))
    );
}

#[test]
fn an_unchanged_part_keeps_its_marker_beside_an_edit_and_an_append() {
    let marked = json!({ "type": "text", "text": "A", "cache_control": { "type": "ephemeral" } });
    let body = json!({ "model": "m", "max_tokens": 5, "messages": [
        { "role": "user", "content": [marked, { "type": "text", "text": "B" }] }] });
    let edited = written(body, |c| {
        c.messages[0].content[1] = "B2".into();
        c.messages[0].content.push("C".into());
    });
    assert_eq!(
        edited.map(|v| v["messages"][0]["content"].clone()).ok(),
        Some(json!([
            marked,
            { "type": "text", "text": "B2" },
            { "type": "text", "text": "C" },
        ]))
    );
}

#[test]
fn a_whole_response_decodes_into_events_per_block() {
    let v = json!({
        "id": "m", "type": "message", "role": "assistant", "model": "c",
        "content": [
            { "type": "thinking", "thinking": "hm", "signature": "sig" },
            { "type": "text", "text": "ok" },
            { "type": "tool_use", "id": "t", "name": "f", "input": { "a": 1 } },
            { "type": "text", "text": "cited", "citations": [{ "type": "char_location" }] },
        ],
        "stop_reason": "tool_use", "stop_sequence": null,
        "usage": { "input_tokens": 2, "output_tokens": 1 },
    });
    let events = decode_body(&v);
    let fields = |v: Value| v.as_object().cloned().unwrap_or_default();
    let signature = fields(json!({ "signature": "sig" }));
    let cited = fields(json!({ "citations": [{ "type": "char_location" }] }));
    let mut totals = Output::usage(Usage {
        input_tokens: 2,
        ..Usage::default()
    });
    if let Output::Usage(u) = &mut totals {
        u.output_tokens += 1;
    }
    assert_eq!(
        events,
        [
            Output::start("m", "c"),
            totals,
            Output::block_start(0, Block::Reasoning),
            Output::reasoning(0, "hm"),
            Output::block_fields(ANTHROPIC, 0, signature),
            Output::block_end(0),
            Output::block_start(1, Block::Text),
            Output::text(1, "ok"),
            Output::block_end(1),
            Output::block_start(2, call("t", "f")),
            Output::arguments(2, "{\"a\":1}"),
            Output::block_end(2),
            // What the kind has no place for comes before the block opens.
            Output::block_fields(ANTHROPIC, 3, cited),
            Output::block_start(3, Block::Text),
            Output::text(3, "cited"),
            Output::block_end(3),
            Output::finished(Finish::ToolUse),
        ]
    );
    // Written again, the whole body is what it was.
    let again = body_of(&events).expect("a body");
    assert_eq!(again["content"], v["content"]);
    assert_eq!(again["stop_reason"], "tool_use");
    assert_eq!(
        again["usage"],
        json!({ "input_tokens": 2, "output_tokens": 1 })
    );
}

#[test]
fn a_cache_marked_part_is_kept_as_the_format_has_it_beside_edited_text() {
    let marked =
        json!({ "type": "text", "text": "the repo", "cache_control": { "type": "ephemeral" } });
    let body = json!({ "model": "m", "max_tokens": 5, "messages": [
        { "role": "user", "content": [marked, { "type": "text", "text": "mine" }] }] });
    let edited = written(body, |c| c.messages[0].content[1] = "MINE".into());
    assert_eq!(
        edited.map(|v| v["messages"][0]["content"].clone()).ok(),
        Some(json!([marked, { "type": "text", "text": "MINE" }]))
    );
}

#[test]
fn an_sse_envelope_is_no_part_of_a_response() {
    let plain = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"secret\"}}\n\n";
    let wrapped = "id: 7\nevent: content_block_delta\nretry: 1500\n: kept\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"secret\"}}\n\n";
    assert_eq!(decode(&[plain]), decode(&[wrapped]));
}

#[test]
fn usage_so_far_is_in_message_start_and_the_totals_in_message_delta() {
    let mut replies = vec![
        Output::start("m", "c"),
        Output::usage(Usage {
            input_tokens: 4,
            ..Usage::default()
        }),
    ];
    replies.extend(text_events(0, "hi"));
    replies.push(Output::usage(Usage {
        output_tokens: 2,
        ..Usage::default()
    }));
    replies.push(Output::finished(Finish::Stop));
    let stream = stream_of(&replies).expect("a stream");
    let events = stream_events(&stream);
    assert_eq!(
        events[0]["message"]["usage"],
        json!({ "input_tokens": 4, "output_tokens": 0 })
    );
    let delta = events.iter().find(|e| e["type"] == "message_delta");
    assert_eq!(
        delta.map(|d| d["usage"].clone()),
        Some(json!({ "input_tokens": 4, "output_tokens": 2 }))
    );
}

#[test]
fn a_finish_released_after_usage_keeps_the_final_totals() {
    let mut replies = vec![Output::start("m", "c")];
    replies.extend(text_events(0, "hi"));
    replies.push(Output::usage(Usage {
        output_tokens: 2,
        ..Usage::default()
    }));
    replies.push(Output::usage(Usage {
        output_tokens: 3,
        ..Usage::default()
    }));
    replies.push(Output::finished(Finish::Length));
    let stream = stream_of(&replies).expect("a stream");
    let events = stream_events(&stream);
    let delta = events.iter().find(|e| e["type"] == "message_delta");
    let delta = delta.cloned().unwrap_or_default();
    assert_eq!(delta["usage"]["output_tokens"], 5);
    assert_eq!(delta["delta"]["stop_reason"], "max_tokens");
}

#[test]
fn dropped_usage_does_not_reappear_in_later_totals() {
    let events = [
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{},\"usage\":{\"output_tokens\":1}}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{},\"usage\":{\"output_tokens\":7}}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{},\"usage\":{\"output_tokens\":7}}\n\n",
    ];
    let mut replies = decode(&events);
    // The second report, which adds 6, is dropped.
    replies.retain(|r| !matches!(r, Output::Usage(u) if u.output_tokens == 6));
    let stream = stream_of(&replies).expect("a stream");
    let events = stream_events(&stream);
    let delta = events.iter().find(|e| e["type"] == "message_delta");
    assert_eq!(
        delta.map(|d| d["usage"]["output_tokens"].clone()),
        Some(json!(1))
    );
}

#[test]
fn usage_never_decreases_and_edits_upward_are_summed() {
    for (totals, edited, expected) in [([7, 1, 0], None, 7), ([1, 7, 7], Some(3), 9)] {
        let events = totals.map(|n| {
            format!("event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{}},\"usage\":{{\"output_tokens\":{n}}}}}\n\n")
        });
        let mut replies = decode(&[&events[0], &events[1], &events[2]]);
        if let Some(amount) = edited {
            replies[0] = Output::usage(Usage {
                output_tokens: amount,
                ..Usage::default()
            });
        }
        let stream = stream_of(&replies).expect("a stream");
        let events = stream_events(&stream);
        let delta = events.iter().find(|e| e["type"] == "message_delta");
        assert_eq!(
            delta.map(|d| d["usage"]["output_tokens"].clone()),
            Some(json!(expected)),
            "{stream}"
        );
    }
}

#[test]
fn unwritable_usage_edits_fail_instead_of_disappearing() {
    // Cache reads are part of the input: more than it is no usage.
    let more = Output::usage(Usage {
        input_tokens: 1,
        cached_input_tokens: 3,
        ..Usage::default()
    });
    assert!(body_of(&[more]).is_err());
}

#[test]
fn cache_usage_is_written_as_the_units_say() {
    let u = Usage {
        input_tokens: 15,
        output_tokens: 1,
        cached_input_tokens: 10,
        cache_write_input_tokens: 2,
        ..Usage::default()
    };
    assert_eq!(
        usage_body(&u, &Map::new()),
        json!({ "input_tokens": 3, "output_tokens": 1,
            "cache_read_input_tokens": 10, "cache_creation_input_tokens": 2 })
    );
    // A cache that was emptied is no field at all.
    let none = Usage::new(15, 1);
    assert_eq!(
        usage_body(&none, &Map::new()),
        json!({ "input_tokens": 15, "output_tokens": 1 })
    );
}

#[test]
fn usage_is_written_as_it_was_read() {
    for u in [
        json!({ "input_tokens": 3, "output_tokens": 1 }),
        json!({ "input_tokens": 3, "output_tokens": 1,
            "cache_read_input_tokens": 10, "cache_creation_input_tokens": 2 }),
        json!({ "input_tokens": 3, "output_tokens": 4,
            "output_tokens_details": { "thinking_tokens": 2 } }),
    ] {
        assert_eq!(
            usage_body(&usage_of(&u).expect("whole counts"), &Map::new()),
            u
        );
    }
}

#[test]
fn a_stop_reason_is_written_as_it_was_read() {
    for (reason, sequence) in [
        ("end_turn", Value::Null),
        ("stop_sequence", json!("END")),
        ("max_tokens", Value::Null),
        ("model_context_window_exceeded", Value::Null),
    ] {
        let v = json!({ "id": "m", "type": "message", "role": "assistant", "model": "c",
            "content": [], "stop_reason": reason, "stop_sequence": sequence,
            "usage": { "input_tokens": 1, "output_tokens": 1 } });
        let again = body_of(&decode_body(&v)).expect("a body");
        assert_eq!(again["stop_reason"], v["stop_reason"]);
        assert_eq!(again["stop_sequence"], v["stop_sequence"]);
        let stream = stream_of(&decode_body(&v)).expect("a stream");
        let delta = stream_events(&stream)
            .into_iter()
            .find(|e| e["type"] == "message_delta");
        let delta = delta.unwrap_or_default();
        assert_eq!(delta["delta"]["stop_reason"], v["stop_reason"]);
        assert_eq!(delta["delta"]["stop_sequence"], v["stop_sequence"]);
    }
}

#[test]
fn blocks_are_numbered_by_their_index() {
    let v = json!({ "id": "m", "type": "message", "role": "assistant", "model": "c",
        "content": [
            { "type": "text", "text": "a" },
            { "type": "tool_use", "id": "t", "name": "f", "input": {} },
            { "type": "text", "text": "b" },
        ], "stop_reason": "tool_use", "usage": { "input_tokens": 1, "output_tokens": 1 } });
    let blocks: Vec<_> = decode_body(&v)
        .into_iter()
        .filter_map(|r| match r {
            Output::BlockStart { block, .. } => Some(block),
            _ => None,
        })
        .collect();
    assert_eq!(blocks, [0, 1, 2]);
}

#[test]
fn a_tool_call_a_filter_turns_to_text_is_gone() {
    let mut replies = vec![Output::start("m", "c")];
    replies.extend(text_events(0, "[removed]"));
    let stream = stream_of(&replies).expect("a stream");
    assert!(
        stream.contains("[removed]") && !stream.contains("tool_use"),
        "{stream}"
    );
}

#[test]
fn a_text_block_with_citations_is_text_with_its_fields_and_events() {
    let out = decode(&[
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"x\",\"citations\":[{\"type\":\"c\"}]}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"citations_delta\",\"citation\":{}}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    ]);
    assert_eq!(out.len(), 5);
    assert!(matches!(&out[0], Output::Native(NativeValue { .. })));
    assert_eq!(out[1], Output::block_start(0, Block::Text));
    assert_eq!(out[2], Output::text(0, "x"));
    // And written as it is, in this format.
    let stream = stream_of(&out).expect("a stream");
    assert!(stream.contains("citations_delta"), "{stream}");
}

#[test]
fn an_event_the_format_cannot_write_fails() {
    // An event of another format that carries content.
    let foreign = Output::native(
        "openai-chat",
        json!({ "choices": [{ "index": 1, "delta": { "content": "B" } }] }),
    );
    assert!(stream_of(std::slice::from_ref(&foreign)).is_err());
    assert!(body_of(&[foreign]).is_err());
    // Text for a block that never opened.
    assert!(stream_of(&[Output::text(0, "x")]).is_err());
    // A block that opens as another kind.
    let events = [
        Output::block_start(0, Block::Text),
        Output::block_start(0, call("t", "f")),
    ];
    assert!(stream_of(&events).is_err());
}

#[test]
fn a_text_part_that_says_more_keeps_it_in_its_extra_and_is_still_plain() {
    let part = json!({ "type": "text", "text": "hi", "citations": [{ "type": "c" }] });
    let chat = AnthropicMessages.decode_json(&json!({ "model": "m", "max_tokens": 1,
        "messages": [{ "role": "user", "content": [part] }] }));
    assert!(chat.messages[0].is_plain());
    assert_eq!(chat.messages[0].text(), "hi");
    let again = AnthropicMessages.encode_request(&chat).map(Value::from);
    assert_eq!(
        again.map(|v| v["messages"][0]["content"][0].clone()).ok(),
        Some(part)
    );
}

#[test]
fn a_system_prompt_of_several_blocks_is_edited_block_by_block() {
    let body = json!({ "model": "m", "max_tokens": 5, "system": [
        { "type": "text", "text": "one" }, { "type": "text", "text": "two sk-1" }],
        "messages": [{ "role": "user", "content": "hi" }] });
    let edited = written(body, |c| c.messages[0].content[1] = "two [x]".into());
    assert_eq!(
        edited.map(|v| v["system"].clone()).ok(),
        Some(json!([{ "type": "text", "text": "one" }, { "type": "text", "text": "two [x]" }]))
    );
}

#[test]
fn a_removed_message_takes_its_cache_marker_with_it() {
    let marked = json!({ "role": "user", "content": [
        { "type": "text", "text": "the repo", "cache_control": { "type": "ephemeral" } }] });
    let body = json!({ "model": "m", "max_tokens": 5, "messages": [
        marked, { "role": "assistant", "content": "ok" }] });
    let edited = written(body, |c| drop(c.messages.remove(0)));
    assert_eq!(
        edited.map(|v| v["messages"].clone()).ok(),
        Some(json!([{ "role": "assistant", "content": "ok" }]))
    );
}

#[test]
fn a_redacted_message_keeps_its_marker_beside_an_inserted_one() {
    let marked =
        json!({ "type": "text", "text": "the repo", "cache_control": { "type": "ephemeral" } });
    let body = json!({ "model": "m", "max_tokens": 5, "messages": [
        { "role": "user", "content": [marked] }] });
    let edited = written(body, |c| {
        c.messages.insert(0, Message::new(Role::User, "first"));
    });
    assert_eq!(
        edited.map(|v| v["messages"].clone()).ok(),
        Some(json!([
            { "role": "user", "content": "first" },
            { "role": "user", "content": [marked] },
        ]))
    );
}

#[test]
fn an_empty_answer_has_no_empty_text_block() {
    let replies = [
        Output::start("m", "c"),
        Output::block_start(0, Block::Text),
        Output::block_end(0),
    ];
    let body = body_of(&replies).expect("a body");
    assert_eq!(body["content"], json!([]));
    let stream = stream_of(&replies).expect("a stream");
    assert!(stream.ends_with("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"));
}

#[test]
fn text_released_late_goes_into_its_block_before_it_stops() {
    // A filter held the end of block 0 and the next text, and released
    // them at the end of the response, in order.
    let replies = [
        Output::start("m", "c"),
        Output::block_start(0, Block::Text),
        Output::text(0, "a"),
        Output::text(0, "b"),
        Output::block_end(0),
    ];
    let stream = stream_of(&replies).expect("a stream");
    let events = stream_events(&stream);
    let kinds: Vec<_> = events.iter().filter_map(|e| e["type"].as_str()).collect();
    assert_eq!(
        kinds,
        [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ]
    );
}

#[test]
fn blocks_that_open_together_are_written_one_at_a_time() {
    // OpenAI streams parallel calls interleaved.
    let replies = [
        Output::block_start(0, call("a", "f")),
        Output::block_start(1, call("b", "g")),
        Output::arguments(1, "{\"b\":"),
        Output::arguments(0, "{}"),
        Output::arguments(1, "1}"),
        Output::block_end(0),
        Output::block_end(1),
    ];
    let stream = stream_of(&replies).expect("a stream");
    let events = stream_events(&stream);
    let seen: Vec<String> = events
        .iter()
        .filter(|e| {
            e["type"]
                .as_str()
                .is_some_and(|t| t.starts_with("content_block"))
        })
        .map(|e| {
            let at = &e["index"];
            match e["type"].as_str().unwrap_or_default() {
                "content_block_delta" => format!("delta{at}:{}", e["delta"]["partial_json"]),
                kind => format!("{kind}{at}"),
            }
        })
        .collect();
    assert_eq!(
        seen,
        [
            "content_block_start0",
            "delta0:\"{}\"",
            "content_block_stop0",
            "content_block_start1",
            "delta1:\"{\\\"b\\\":1}\"",
            "content_block_stop1",
        ]
    );
}

#[test]
fn a_stream_error_after_the_finish_keeps_the_finish_and_ends_last() {
    let mut enc = EventStream::default();
    let mut out = BytesMut::new();
    let mut replies = vec![Output::start("m", "c")];
    replies.extend(text_events(0, "hi"));
    replies.push(Output::finished(Finish::Stop));
    for reply in &replies {
        assert!(enc.encode_into(reply, &mut out).is_ok());
    }
    let _ = enc.end_into(&Err(respan_gateway_core::Fault::upstream("boom")), &mut out);
    let text = String::from_utf8_lossy(&out);
    let delta = text.find("message_delta").unwrap_or(usize::MAX);
    let error = text.find("event: error").unwrap_or(0);
    assert!(delta < error, "{text}");
    assert!(!text.contains("message_stop"), "{text}");
}

#[test]
fn extra_fields_of_the_message_survive_in_the_same_format() {
    let body = json!({ "id": "m", "type": "message", "role": "assistant", "model": "c",
        "content": [{ "type": "text", "text": "hi" }], "stop_reason": "end_turn",
        "stop_sequence": null, "container": { "id": "k" },
        "usage": { "input_tokens": 1, "output_tokens": 1 } });
    let events = decode_body(&body);
    let Some(Output::Start(start)) = events.first() else {
        panic!("{events:?}");
    };
    assert_eq!(
        start.native,
        Some(NativeFields::new(
            ANTHROPIC,
            json!({ "container": { "id": "k" } })
                .as_object()
                .cloned()
                .unwrap_or_default()
        ))
    );
    assert_eq!(
        body_of(&events).expect("a body")["container"],
        json!({ "id": "k" })
    );
    let stream = stream_of(&events).expect("a stream");
    assert_eq!(
        stream_events(&stream)[0]["message"]["container"],
        json!({ "id": "k" })
    );
}

#[test]
fn anthrope_sse_to_unary_keeps_text_tool_text_and_tool_text_order() {
    let request = http::Request::new(Bytes::from_static(b"{\"stream\":false}"));
    let call = http::Request::new(()).into_parts().0;
    for (parts, expected) in [
        (
            vec![
                json!({"type":"text","text":"before"}),
                json!({"type":"tool_use","id":"t","name":"lookup","input":{"q":"a"}}),
                json!({"type":"text","text":"after"}),
            ],
            vec!["text", "tool_use", "text"],
        ),
        (
            vec![
                json!({"type":"tool_use","id":"t","name":"lookup","input":{"q":"a"}}),
                json!({"type":"text","text":"after"}),
            ],
            vec!["tool_use", "text"],
        ),
    ] {
        let mut events = vec![
            json!({"type":"message_start","message":{"id":"m","model":"claude-x","usage":{"input_tokens":3,"output_tokens":0}}}),
        ];
        for (index, part) in parts.iter().enumerate() {
            events.push(json!({"type":"content_block_start","index":index,"content_block": match part["type"].as_str() {
                Some("text") => json!({"type":"text","text":""}),
                _ => json!({"type":"tool_use","id":part["id"],"name":part["name"],"input":{}}),
            }}));
            let delta = match part["type"].as_str() {
                Some("text") => json!({"type":"text_delta","text":part["text"]}),
                _ => {
                    json!({"type":"input_json_delta","partial_json":part["input"].to_string()})
                }
            };
            events.push(json!({"type":"content_block_delta","index":index,"delta":delta}));
            events.push(json!({"type":"content_block_stop","index":index}));
        }
        events.extend([
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":2}}),
            json!({"type":"message_stop"}),
        ]);
        let body: String = events
            .iter()
            .map(|event| {
                format!(
                    "event: {}\ndata: {event}\n\n",
                    event["type"].as_str().unwrap_or_default()
                )
            })
            .collect();
        let mut head = http::Response::new(()).into_parts().0;
        head.headers
            .insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
        let replies = AnthropicMessages
            .decode_response(http::Response::from_parts(head, body.into()), &call)
            .expect("decode SSE");
        let rendered = AnthropicMessages
            .encode_response(&request, &replies)
            .expect("render unary");
        let rendered: Value = serde_json::from_slice(rendered.body()).expect("JSON answer");
        let content = rendered["content"].as_array().expect("blocks");
        let types: Vec<_> = content.iter().filter_map(|b| b["type"].as_str()).collect();
        assert_eq!(types, expected);
        assert_eq!(
            rendered["usage"],
            json!({"input_tokens":3,"output_tokens":2})
        );
        assert_eq!(rendered["stop_reason"], "tool_use");
        let texts: Vec<_> = content.iter().filter_map(|b| b["text"].as_str()).collect();
        assert_eq!(
            texts,
            if parts.len() == 3 {
                vec!["before", "after"]
            } else {
                vec!["after"]
            }
        );
        let tool = content
            .iter()
            .find(|b| b["type"] == "tool_use")
            .expect("tool");
        assert_eq!(tool["input"], json!({"q":"a"}));
    }
}

#[test]
fn a_large_content_edit_keeps_every_part_as_it_was_without_matching_them() {
    let old: Value = (0..10_000)
        .map(|n| {
            json!({
                "type": "text", "text": n.to_string(), "cache_control": {"type": "ephemeral"}
            })
        })
        .collect();
    let mut content = decode_content::<AnthropicMessages>(&old);
    content[5_000] = Content::from("edited");
    let written = encode_content::<AnthropicMessages>(&content).expect("content");
    assert_eq!(written.as_array().expect("an array").len(), 10_000);
    assert_eq!(written[5_000], json!({"type":"text","text":"edited"}));
    assert_eq!(written[4_999], old[4_999]);
    assert_eq!(written[5_001], old[5_001]);
}

#[test]
fn large_content_in_another_order_is_written_in_that_order() {
    let old: Value = (0..1_000)
        .map(|n| json!({"type": "text", "text": n.to_string()}))
        .collect();
    let was = decode_content::<AnthropicMessages>(&old);
    let now: Vec<_> = was.iter().rev().cloned().collect();
    let written = encode_content::<AnthropicMessages>(&now).expect("content");
    let reversed: Vec<Value> = old
        .as_array()
        .expect("an array")
        .iter()
        .rev()
        .cloned()
        .collect();
    assert_eq!(written, Value::Array(reversed));
}
