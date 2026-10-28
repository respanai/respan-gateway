use super::*;

use respan_gateway_conversation::{
    Block, Effort, Media, MediaSource, Message, Reasoning, Start, Tool, ToolChoice, Usage,
};
use respan_gateway_core::NativeFields;
use respan_gateway_eventstream::{Header, Message as Frame};
use respan_gateway_http::Decoder;
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::{self, HeaderValue, header::CONTENT_TYPE};

fn chat(messages: impl IntoIterator<Item = Message>) -> Input {
    let mut chat = Input::new(messages);
    chat.model = Some("amazon.nova-lite-v1:0".into());
    chat
}

fn user(text: &str) -> Message {
    Message::new(Role::User, text)
}

fn assistant(text: &str) -> Message {
    Message::new(Role::Assistant, text)
}

fn written(chat: &Input) -> Value {
    BedrockConverse
        .encode_request(chat)
        .expect("a request")
        .into()
}

fn refused(chat: &Input) -> String {
    let fault = BedrockConverse.encode_request(chat).expect_err("a refusal");
    fault.message().to_owned()
}

fn media(mime: &str, bytes: &'static [u8]) -> Media {
    Media::new(mime, MediaSource::Bytes(Bytes::from_static(bytes)))
}

#[test]
fn a_request_is_system_messages_and_inference_config() {
    let mut chat = chat([
        Message::new(Role::System, "be brief"),
        Message::new(Role::Developer, "no lists"),
        user("hi"),
        user("  "),
        assistant("hello"),
        user("again"),
        user("and"),
    ]);
    chat.options.max_output_tokens = Some(100);
    chat.options.temperature = Some(0.5);
    chat.options.top_p = Some(0.9);
    chat.options.stop = vec!["END".into()];
    assert_eq!(
        written(&chat),
        json!({
            "system": [{ "text": "be brief" }, { "text": "no lists" }],
            "messages": [
                { "role": "user", "content": [{ "text": "hi" }] },
                { "role": "assistant", "content": [{ "text": "hello" }] },
                // Converse's roles alternate: a turn joins the one before it.
                { "role": "user", "content": [{ "text": "again" }, { "text": "and" }] },
            ],
            "inferenceConfig": {
                "maxTokens": 100, "temperature": 0.5, "topP": 0.9, "stopSequences": ["END"]
            },
        })
    );
}

#[test]
fn media_is_bytes_and_a_url_is_refused() {
    let mut message = user("what is this");
    message
        .content
        .push(Content::Image(media("image/png", b"\x89PNG")));
    let mut pdf = media("application/pdf", b"%PDF");
    pdf.name = Some("my  report_v2.pdf".into());
    message.content.push(Content::Document(pdf));
    let blocks = written(&chat([message]))["messages"][0]["content"].clone();
    assert_eq!(
        blocks,
        json!([
            { "text": "what is this" },
            { "image": { "format": "png", "source": { "bytes": "iVBORw==" } } },
            { "document": { "format": "pdf", "name": "my report-v2-pdf", "source": { "bytes": "JVBERg==" } } },
        ])
    );
    let url = Media::new("image/png", MediaSource::Url("https://x.io/a.png".into()));
    let mut remote = user("see");
    remote.content.push(Content::Image(url));
    assert!(refused(&chat([remote])).contains("data URL"));
    let mut odd = user("see");
    odd.content.push(Content::Image(media("image/tiff", b"II")));
    assert!(refused(&chat([odd])).contains("image/tiff"));
}

#[test]
fn tools_a_call_and_its_result_are_blocks() {
    let mut call = assistant("");
    call.content = vec![Content::tool_call("t1", "lookup", "{\"id\":7}")];
    let mut answer = user("");
    answer.content = vec![Content::tool_result("t1", vec!["found".into()], false)];
    let mut failed = user("");
    failed.content = vec![Content::tool_result("t2", vec!["boom".into()], true)];
    failed
        .content
        .push(Content::tool_result("t3", vec!["  ".into()], false));
    let mut chat = chat([user("find 7"), call, answer, failed]);
    let mut tool = Tool::new("lookup", json!({ "type": "object" }));
    tool.description = Some("finds".into());
    chat.tools = vec![tool];
    chat.options.tool_choice = Some(ToolChoice::Tool("lookup".into()));
    let v = written(&chat);
    assert_eq!(
        v["toolConfig"],
        json!({
            "tools": [{ "toolSpec": {
                "name": "lookup", "description": "finds",
                "inputSchema": { "json": { "type": "object" } }
            } }],
            "toolChoice": { "tool": { "name": "lookup" } },
        })
    );
    assert_eq!(
        v["messages"][1]["content"],
        json!([{ "toolUse": { "toolUseId": "t1", "name": "lookup", "input": { "id": 7 } } }])
    );
    assert_eq!(
        v["messages"][2]["content"],
        json!([
            { "toolResult": { "toolUseId": "t1", "content": [{ "text": "found" }] } },
            { "toolResult": { "toolUseId": "t2", "content": [{ "text": "boom" }], "status": "error" } },
            { "toolResult": { "toolUseId": "t3", "content": [{ "text": "(no output)" }] } },
        ])
    );
}

#[test]
fn a_tool_choice_is_auto_any_or_one_tool() {
    let with = |choice| {
        let mut chat = chat([user("hi")]);
        chat.tools = vec![Tool::new("f", Value::Null)];
        chat.options.tool_choice = choice;
        written(&chat)["toolConfig"].clone()
    };
    assert_eq!(with(None).get("toolChoice"), None);
    assert_eq!(with(Some(ToolChoice::Auto)).get("toolChoice"), None);
    assert_eq!(
        with(Some(ToolChoice::Required))["toolChoice"],
        json!({ "any": {} })
    );
    assert_eq!(with(Some(ToolChoice::None)), Value::Null);
    assert_eq!(
        with(None)["tools"][0]["toolSpec"]["inputSchema"],
        json!({ "json": { "type": "object" } })
    );
}

#[test]
fn a_reasoning_budget_is_the_thinking_field_and_signed_reasoning_goes_back() {
    let signed = NativeFields::new(
        BEDROCK,
        json!({ "signature": "sig" })
            .as_object()
            .cloned()
            .unwrap_or_default(),
    );
    let mut turn = assistant("done");
    turn.content.insert(
        0,
        Content::Reasoning {
            text: "hm".into(),
            native: Some(signed),
        },
    );
    turn.content.insert(
        0,
        Content::Reasoning {
            text: "unsigned".into(),
            native: None,
        },
    );
    let mut chat = chat([user("think"), turn, user("more")]);
    chat.options.reasoning = Some(Reasoning::budget(2048));
    let v = written(&chat);
    assert_eq!(
        v["additionalModelRequestFields"],
        json!({ "thinking": { "type": "enabled", "budget_tokens": 2048 } })
    );
    assert_eq!(
        v["messages"][1]["content"],
        json!([
            { "reasoningContent": { "reasoningText": { "text": "hm", "signature": "sig" } } },
            { "text": "done" },
        ])
    );
    chat.options.reasoning = Some(Reasoning::effort(Effort::Off));
    assert_eq!(
        written(&chat)["additionalModelRequestFields"],
        json!({ "thinking": { "type": "disabled" } })
    );
}

#[test]
fn what_converse_has_no_field_for_is_refused() {
    let base = || chat([user("hi")]);
    let mut c = base();
    c.options.top_k = Some(5);
    assert!(refused(&c).contains("top_k"));
    let mut c = base();
    c.user = Some("u".into());
    assert!(refused(&c).contains("end user"));
    let mut c = base();
    c.options.reasoning = Some(Reasoning::effort(Effort::High));
    assert!(refused(&c).contains("budget"));
    let mut c = base();
    c.options.parallel_tool_calls = Some(false);
    assert!(refused(&c).contains("one tool call"));
    let c = chat([user("a"), Message::new(Role::System, "late")]);
    assert!(refused(&c).contains("system message"));
    let mut c = base();
    c.native = Some(NativeFields::new(
        "openai-chat",
        json!({ "seed": 1 })
            .as_object()
            .cloned()
            .unwrap_or_default(),
    ));
    assert!(refused(&c).contains("seed"));
}

#[test]
fn the_model_is_the_path_escaped_whole() {
    let target = |model: Option<&str>, stream| {
        let mut c = chat([user("hi")]);
        c.model = model.map(Into::into);
        BedrockConverse.target(&c, stream)
    };
    assert_eq!(
        target(Some("us.amazon.nova-lite-v1:0"), false)
            .ok()
            .flatten()
            .as_deref(),
        Some("/model/us.amazon.nova-lite-v1%3A0/converse")
    );
    let arn = "arn:aws:bedrock:us-east-1:1:application-inference-profile/abc";
    assert_eq!(
        target(Some(arn), true).ok().flatten().as_deref(),
        Some(
            "/model/arn%3Aaws%3Abedrock%3Aus-east-1%3A1%3Aapplication-inference-profile%2Fabc/converse-stream"
        )
    );
    assert!(target(None, false).is_err());
}

/// An event message of the stream.
fn event(kind: &str, payload: Value) -> Bytes {
    Frame {
        headers: vec![
            (":event-type".into(), Header::String(kind.into())),
            (
                ":content-type".into(),
                Header::String("application/json".into()),
            ),
            (":message-type".into(), Header::String("event".into())),
        ],
        payload: payload.to_string().into(),
    }
    .encode()
}

fn decode_chunks(chunks: &[Bytes]) -> Result<Vec<Output>, Fault> {
    let mut decoder = ConverseDecoder::default();
    let mut out = Vec::new();
    for chunk in chunks {
        decoder.decode(chunk.clone(), &mut out)?;
    }
    decoder.end(Ok(()), &mut out)?;
    Ok(out)
}

fn native(v: Value) -> Output {
    Output::native(BEDROCK, v)
}

fn stream_of(frames: Vec<Bytes>) -> Vec<u8> {
    frames.iter().flat_map(|f| f.to_vec()).collect()
}

fn text_stream() -> Vec<Bytes> {
    vec![
        event("messageStart", json!({ "role": "assistant" })),
        event(
            "contentBlockDelta",
            json!({ "contentBlockIndex": 0, "delta": { "text": "Hel" } }),
        ),
        event(
            "contentBlockDelta",
            json!({ "contentBlockIndex": 0, "delta": { "text": "lo" } }),
        ),
        event("contentBlockStop", json!({ "contentBlockIndex": 0 })),
        event("messageStop", json!({ "stopReason": "end_turn" })),
        event(
            "metadata",
            json!({ "usage": { "inputTokens": 10, "outputTokens": 5, "totalTokens": 15,
                    "cacheReadInputTokens": 4, "cacheWriteInputTokens": 2 },
                    "metrics": { "latencyMs": 40 } }),
        ),
    ]
}

#[test]
fn a_stream_is_blocks_a_finish_and_usage() {
    let mut usage = Usage::new(16, 5);
    usage.cached_input_tokens = 4;
    usage.cache_write_input_tokens = 2;
    assert_eq!(
        decode_chunks(&text_stream()).expect("events"),
        [
            Output::Start(Start::new("", "")),
            Output::block_start(0, Block::Text),
            Output::text(0, "Hel"),
            Output::text(0, "lo"),
            Output::block_end(0),
            Output::finished(Finish::Stop),
            Output::Usage(usage),
            native(
                json!({ "type": "metadata_fields", "fields": { "metrics": { "latencyMs": 40 } } })
            ),
        ]
    );
}

#[test]
fn tool_use_and_reasoning_blocks_open_as_converse_sends_them() {
    let events = decode_chunks(&[
        event("contentBlockDelta", json!({ "contentBlockIndex": 0, "delta": { "reasoningContent": { "text": "hm" } } })),
        event("contentBlockDelta", json!({ "contentBlockIndex": 0, "delta": { "reasoningContent": { "signature": "sig" } } })),
        event("contentBlockStop", json!({ "contentBlockIndex": 0 })),
        event("contentBlockStart", json!({ "contentBlockIndex": 1, "start": { "toolUse": { "toolUseId": "t1", "name": "lookup" } } })),
        event("contentBlockDelta", json!({ "contentBlockIndex": 1, "delta": { "toolUse": { "input": "{\"id\":" } } })),
        event("contentBlockDelta", json!({ "contentBlockIndex": 1, "delta": { "toolUse": { "input": "7}" } } })),
        event("contentBlockStop", json!({ "contentBlockIndex": 1 })),
        event("messageStop", json!({ "stopReason": "tool_use" })),
    ])
    .expect("events");
    let signature = json!({ "signature": "sig" })
        .as_object()
        .cloned()
        .unwrap_or_default();
    let call = Block::ToolCall {
        id: "t1".into(),
        name: "lookup".into(),
    };
    assert_eq!(
        events[1..],
        [
            Output::block_start(0, Block::Reasoning),
            Output::reasoning(0, "hm"),
            Output::block_fields(BEDROCK, 0, signature),
            Output::block_end(0),
            Output::block_start(1, call),
            Output::arguments(1, "{\"id\":"),
            Output::arguments(1, "7}"),
            Output::block_end(1),
            Output::finished(Finish::ToolUse),
        ]
    );
}

#[test]
fn a_cut_stream_and_a_stream_with_no_stop_fail() {
    let whole = stream_of(text_stream());
    let cut = Bytes::copy_from_slice(&whole[..whole.len() - 3]);
    let fault = decode_chunks(&[cut]).expect_err("a cut");
    assert_eq!(fault.code(), "truncated");
    let fault = decode_chunks(&text_stream()[..3]).expect_err("no stop");
    assert_eq!(fault.code(), "truncated");
}

#[test]
fn an_exception_in_the_stream_is_an_upstream_fault() {
    let exception = Frame {
        headers: vec![
            (
                ":exception-type".into(),
                Header::String("throttlingException".into()),
            ),
            (":message-type".into(), Header::String("exception".into())),
        ],
        payload: Bytes::from_static(b"{\"message\":\"slow down\"}"),
    };
    let chunks = [text_stream()[0].clone(), exception.encode()];
    let fault = decode_chunks(&chunks).expect_err("a fault");
    assert_eq!(fault.message(), "slow down");
    assert_eq!(fault.code(), "throttlingException");
    assert_eq!(
        fault
            .get::<respan_gateway_http_faults::Status>()
            .map(|s| s.0.as_u16()),
        Some(429)
    );
}

#[test]
fn a_whole_body_is_the_same_events() {
    let body = json!({
        "output": { "message": { "role": "assistant", "content": [
            { "reasoningContent": { "reasoningText": { "text": "hm", "signature": "sig" } } },
            { "text": "Hello" },
            { "toolUse": { "toolUseId": "t1", "name": "lookup", "input": { "id": 7 } } },
            { "citationsContent": { "citations": [] } },
        ] } },
        "stopReason": "tool_use",
        "usage": { "inputTokens": 10, "outputTokens": 5, "totalTokens": 15 },
        "metrics": { "latencyMs": 40 },
        "additionalModelResponseFields": { "x": 1 },
    });
    let mut events = Vec::new();
    BedrockConverse
        .decode_body(&body, &mut events)
        .expect("events");
    let signature = json!({ "signature": "sig" })
        .as_object()
        .cloned()
        .unwrap_or_default();
    let call = Block::ToolCall {
        id: "t1".into(),
        name: "lookup".into(),
    };
    assert_eq!(
        events,
        [
            Output::Start(Start::new("", "")),
            Output::Usage(Usage::new(10, 5)),
            Output::block_start(0, Block::Reasoning),
            Output::reasoning(0, "hm"),
            Output::block_fields(BEDROCK, 0, signature),
            Output::block_end(0),
            Output::block_start(1, Block::Text),
            Output::text(1, "Hello"),
            Output::block_end(1),
            Output::block_start(2, call),
            Output::arguments(2, "{\"id\":7}"),
            Output::block_end(2),
            native(
                json!({ "type": "content_block", "index": 3, "block": { "citationsContent": { "citations": [] } } })
            ),
            Output::finished(Finish::ToolUse),
            native(
                json!({ "type": "stop_fields", "fields": { "additionalModelResponseFields": { "x": 1 } } })
            ),
            native(
                json!({ "type": "metadata_fields", "fields": { "metrics": { "latencyMs": 40 } } })
            ),
        ]
    );
}

fn head(status: u16, content_type: &'static str) -> http::response::Parts {
    let mut head = http::Response::new(()).into_parts().0;
    head.status = http::StatusCode::from_u16(status).unwrap_or_default();
    head.headers
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    head
}

#[test]
fn a_response_is_read_by_its_media_type() {
    let call = http::Request::post("/model/m/converse-stream")
        .body(())
        .expect("a call")
        .into_parts()
        .0;
    let answer = |content_type, body: Vec<u8>| {
        let (head, ()) = http::Response::from_parts(head(200, content_type), ()).into_parts();
        BedrockConverse.decode_response(http::Response::from_parts(head, Bytes::from(body)), &call)
    };
    let events = answer(
        "application/vnd.amazon.eventstream",
        stream_of(text_stream()),
    )
    .expect("a stream");
    assert!(events.contains(&Output::text(0, "Hel")));
    let body = json!({ "output": { "message": { "content": [{ "text": "hi" }] } },
        "stopReason": "end_turn", "usage": { "inputTokens": 1, "outputTokens": 1 } });
    let events = answer("application/json", body.to_string().into_bytes()).expect("a body");
    assert!(events.contains(&Output::text(0, "hi")));
    let refused = answer("application/json", b"{\"message\":\"no\"}".to_vec());
    assert!(refused.is_err_and(|f| f.message().contains("success is not a response")));
}

#[test]
fn a_refusal_is_read_as_bedrock_words_it() {
    let mut refused =
        http::Response::new(Bytes::from_static(b"{\"message\":\"Too many requests\"}"));
    *refused.status_mut() = http::StatusCode::TOO_MANY_REQUESTS;
    let fault = crate::schema::answer(&BedrockConverse, refused);
    assert_eq!(fault.message(), "Too many requests");
    assert_eq!(
        fault
            .get::<respan_gateway_http_faults::Status>()
            .map(|s| s.0.as_u16()),
        Some(429)
    );
}

#[test]
fn a_cache_marker_of_any_format_is_a_cache_point_after_its_block() {
    let mut marked = user("long context");
    if let Content::Text { native, .. } = &mut marked.content[0] {
        let marker = json!({ "cache_control": { "type": "ephemeral" } });
        *native = Some(NativeFields::new(
            "anthropic-messages",
            marker.as_object().cloned().unwrap_or_default(),
        ));
    }
    let chat = chat([marked, user("question")]);
    assert_eq!(
        written(&chat)["messages"][0]["content"],
        json!([{ "text": "long context" }, { "cachePoint": { "type": "default" } }, { "text": "question" }])
    );
}
