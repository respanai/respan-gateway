use super::*;
use respan_gateway_core::Fault;

use respan_gateway_http::{Decoder, Sse};

use respan_gateway_http::bytes::{Bytes, BytesMut};

use crate::anthropic::AnthropicMessages;
use crate::schema::Held;
use respan_gateway_conversation::{Content, Media, MediaSource, Message, Tool, ToolChoice};
use respan_gateway_core::NativeFields;
use respan_gateway_core::{Details, ErrorCode};

use respan_gateway_http::http::StatusCode;
use respan_gateway_http_faults::Status;

/// The events of `chunks`, and how the stream ended.
fn decode_all(chunks: &[&str]) -> (Vec<Output>, Result<(), Fault>) {
    let mut d = Sse::new(OpenAiChat.decoder());
    let mut out = Vec::new();
    for c in chunks {
        if let Err(f) = d.decode(Bytes::from(c.to_string()), &mut out) {
            return (out, Err(f));
        }
    }
    let end = d.end(Ok(()), &mut out);
    (out, end)
}

/// The events of one whole body.
fn decode_body(v: &Value) -> Vec<Output> {
    let mut out = Vec::new();
    OpenAiChat
        .decoder()
        .body(v, &mut out)
        .expect("a whole response");
    out
}

/// `replies` as a stream a client that asked for usage as `usage` gets,
/// whole, and how it ended.
fn stream_of(replies: &[Output], usage: bool) -> Result<String, Fault> {
    let mut enc = Chunks::new(usage);
    let mut out = BytesMut::new();
    for reply in replies {
        enc.encode_into(reply, &mut out)?;
    }
    enc.end_into(&Ok(()), &mut out)?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// `replies` as a whole body.
fn body_of(replies: &[Output]) -> Result<Value, Fault> {
    let mut whole = Assembled::default();
    replies.iter().try_for_each(|r| whole.push(r))?;
    OpenAiChat.encode_whole(&whole)
}

/// The JSON of each data line, but `[DONE]`.
fn chunks_of(stream: &str) -> Vec<Value> {
    let data = stream
        .split("\n\n")
        .filter_map(|e| e.strip_prefix("data: "));
    data.filter_map(|d| serde_json::from_str(d).ok()).collect()
}

fn text_events(block: u32, text: &str) -> Vec<Output> {
    vec![
        Output::block_start(block, Block::Text),
        Output::text(block, text),
        Output::block_end(block),
    ]
}

fn call(id: &str, name: &str) -> Block {
    Block::ToolCall {
        id: id.into(),
        name: name.into(),
    }
}

#[test]
fn a_start_is_pushed_only_with_the_first_content_and_dropped_at_a_failure() {
    let role = "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}],\"usage\":{\"prompt_tokens\":2}}\n\n";
    let text =
        "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"}}]}\n\n";
    let error = "data: {\"error\":{\"message\":\"Overloaded\"}}\n\n";
    let batches = |events: &[&str]| {
        let mut d = Sse::new(Held::new(OpenAiChat.decoder()));
        let batches: Vec<(Vec<Output>, bool)> = events
            .iter()
            .map(|e| {
                let mut out = Vec::new();
                let failed = d.decode(Bytes::from(e.to_string()), &mut out).is_err();
                (out, failed)
            })
            .collect();
        batches
    };
    let content = batches(&[role, ": ping\n\n", text]);
    assert!(content[0].0.is_empty() && content[1].0.is_empty());
    // The start, the usage held behind it, and the text, in order.
    let kinds: Vec<bool> = content[2].0.iter().map(Output::is_content).collect();
    assert_eq!(kinds, [true, false, true, true]);
    assert!(matches!(&content[2].0[0], Output::Start(s) if s.id == "c1"));
    // A failure first leaves only the usage held, saying nothing.
    let failed = batches(&[role, error]);
    assert!(failed[1].1);
    assert_eq!(failed[1].0.len(), 1);
    let held = &failed[1].0[0];
    assert!(
        matches!(held, Output::Usage(u) if u.input_tokens == 2),
        "{held:?}"
    );
}

#[test]
fn a_stream_decodes_into_events() {
    let (out, end) = decode_all(&[
        "data: {\"id\":\"x\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"He",
        "llo\"}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1}}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n\n",
        "data: [DONE]\n\n",
    ]);
    assert!(end.is_ok());
    let mut first = Output::usage(Usage {
        input_tokens: 3,
        ..Usage::default()
    });
    if let Output::Usage(u) = &mut first {
        u.output_tokens += 1;
    }
    assert_eq!(
        out,
        [
            Output::start("x", "m"),
            Output::block_start(0, Block::Text),
            Output::text(0, "Hello"),
            // Totals repeated in a later chunk add only what changed.
            first,
            Output::block_end(0),
            Output::finished(Finish::Stop),
            Output::usage(Usage {
                output_tokens: 1,
                ..Usage::default()
            }),
        ]
    );
}

/// Chunks as OpenAI sends them, with the fields it sets to null.
#[test]
fn real_openai_chunks_decode_to_text_and_tool_calls() {
    let (out, _) = decode_all(&[
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt\",\"system_fingerprint\":\"fp\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\",\"refusal\":null},\"logprobs\":null,\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"logprobs\":null,\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"f\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{},\"logprobs\":null,\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    ]);
    let mut start = Output::start("c1", "gpt");
    if let Output::Start(s) = &mut start {
        let fields = json!({ "created": 1, "system_fingerprint": "fp" });
        let fields = fields.as_object().cloned().unwrap_or_default();
        s.native = Some(NativeFields::new(OPENAI, fields));
    }
    assert_eq!(
        out,
        [
            start,
            // An empty text opens no block.
            Output::block_start(0, Block::Text),
            Output::text(0, "Hi"),
            // A tool call ends the text before it, and starts its own.
            Output::block_end(0),
            Output::block_start(1, call("call_1", "f")),
            Output::block_end(1),
            Output::finished(Finish::ToolUse),
        ]
    );
}

#[test]
fn a_native_finish_is_written_as_its_fallback() {
    let full = Finish::native_or("other", "context_full", Finish::Length);
    let body = body_of(&[Output::start("x", "m"), Output::finished(full)]).expect("a body");
    assert_eq!(body["choices"][0]["finish_reason"], "length");
    let paused = Finish::native("other", "pause_turn");
    assert!(body_of(&[Output::start("x", "m"), Output::finished(paused)]).is_err());
}

#[test]
fn calls_without_an_index_are_told_apart_by_their_id() {
    let (out, _) = decode_all(&[
        "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"id\":\"a\",\"function\":{\"name\":\"f\",\"arguments\":\"{\\\"x\\\":\"}}]}}]}\n\n",
        "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"function\":{\"arguments\":\"1}\"}}]}}]}\n\n",
        "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"id\":\"b\",\"function\":{\"name\":\"g\",\"arguments\":\"{\\\"y\\\":2}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    ]);
    assert_eq!(
        out[1..],
        [
            Output::block_start(0, call("a", "f")),
            Output::arguments(0, "{\"x\":"),
            Output::arguments(0, "1}"),
            Output::block_start(1, call("b", "g")),
            Output::arguments(1, "{\"y\":2}"),
            Output::block_end(0),
            Output::block_end(1),
            Output::finished(Finish::ToolUse),
        ]
    );
}

#[test]
fn blocks_are_numbered_in_the_order_they_open() {
    let (out, _) = decode_all(&[
        "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"}}]}\n\n",
        "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":3,\"id\":\"a\",\"function\":{\"name\":\"f\",\"arguments\":\"{\"}},{\"index\":1,\"id\":\"b\",\"function\":{\"name\":\"g\",\"arguments\":\"\"}}]}}]}\n\n",
        "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":3,\"function\":{\"arguments\":\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    ]);
    assert_eq!(
        out[1..],
        [
            Output::block_start(0, Block::Text),
            Output::text(0, "Hi"),
            Output::block_end(0),
            // Each call, by its `index`, is a block, as it opens.
            Output::block_start(1, call("a", "f")),
            Output::arguments(1, "{"),
            Output::block_start(2, call("b", "g")),
            Output::arguments(1, "}"),
            Output::block_end(1),
            Output::block_end(2),
            Output::finished(Finish::ToolUse),
        ]
    );
}

#[test]
fn a_choice_with_logprobs_is_other_content() {
    let (out, _) = decode_all(&[
        "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"logprobs\":{\"content\":[]}}]}\n\n",
    ]);
    // Its text is text, as in a whole response, so a filter sees it; the
    // rest is other content, which comes first.
    let rest = json!({ "choices": [{ "index": 0, "logprobs": { "content": [] } }] });
    assert_eq!(
        out[1..],
        [
            Output::native(OPENAI, rest),
            Output::block_start(0, Block::Text),
            Output::text(0, "Hi"),
        ]
    );
}

#[test]
fn a_choices_unknown_field_stays_native_and_goes_back_as_it_came() {
    let chunk = "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\",\"x_vendor\":\"stop\",\"x_null\":null}]}\n\n";
    let (out, _) = decode_all(&[chunk]);
    let rest = json!({ "choices": [{ "index": 0, "x_vendor": "stop" }] });
    assert_eq!(
        out[1..],
        [Output::native(OPENAI, rest), Output::finished(Finish::Stop)]
    );
    let stream = stream_of(&out, false).expect("a stream");
    assert!(stream.contains("\"x_vendor\":\"stop\""), "{stream}");
    // Another format drops it: only a client of this one could read it.
    let mut whole = Assembled::default();
    out.iter()
        .try_for_each(|r| whole.push(r))
        .expect("assembled");
    let body = AnthropicMessages.encode_whole(&whole).expect("a body");
    assert!(!body.to_string().contains("x_vendor"), "{body}");
}

#[test]
fn openai_chat_reads_reasoning_content_and_relays_a_vendors_field() {
    let chunks = [
        "data: {\"id\":\"x\",\"x_vendor\":{\"usage\":{\"prompt_tokens\":3}},\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"hm\"}}]}\n\n",
        "data: [DONE]\n\n",
    ];
    let (out, end) = decode_all(&chunks);
    assert!(end.is_ok());
    // The vendor's object is no usage, and stays in the start's fields;
    // `reasoning_content` is reasoning, where every vendor that reasons
    // over Chat Completions says it.
    let Some(Output::Start(start)) = out.first() else {
        panic!("{out:?}");
    };
    assert_eq!(
        start.native.fields()["x_vendor"],
        json!({ "usage": { "prompt_tokens": 3 } })
    );
    assert_eq!(
        out[1..],
        [
            Output::block_start(0, Block::Reasoning),
            Output::reasoning(0, "hm"),
            Output::block_end(0),
        ]
    );
    // Both go back to a client of the format as they came.
    let stream = stream_of(&out, false).expect("a stream");
    assert!(stream.contains("\"x_vendor\":{\"usage\""), "{stream}");
    assert!(stream.contains("\"reasoning_content\":\"hm\""), "{stream}");
    // A request's reasoning is a message field like any other.
    let body = json!({ "model": "m", "messages": [
        { "role": "assistant", "content": "ok", "reasoning_content": "hm" },
    ]});
    let chat = OpenAiChat.decode_json(&body);
    assert_eq!(chat.messages[0].content, [Content::from("ok")]);
    assert_eq!(
        chat.messages[0].native.fields().get("reasoning_content"),
        Some(&json!("hm"))
    );
    assert_eq!(
        written(body, |_| {})
            .ok()
            .map(|v| v["messages"][0]["reasoning_content"].clone()),
        Some(json!("hm"))
    );
}

#[test]
fn reasoning_content_opens_a_reasoning_block_before_the_text() {
    let chunks = [
        "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"hm\"}}]}\n\n",
        "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n",
    ];
    let (out, _) = decode_all(&chunks);
    assert_eq!(
        out[1..],
        [
            Output::block_start(0, Block::Reasoning),
            Output::reasoning(0, "hm"),
            Output::block_end(0),
            Output::block_start(1, Block::Text),
            Output::text(1, "ok"),
            Output::block_end(1),
            Output::finished(Finish::Stop),
        ]
    );
    // A whole body's message says it the same way.
    let body = json!({ "id": "x", "choices": [{ "index": 0, "finish_reason": "stop",
        "message": { "role": "assistant", "content": "ok", "reasoning_content": "hm" } }] });
    assert_eq!(decode_body(&body)[1..], out[1..]);
}

#[test]
fn only_the_first_choice_finishes_the_response() {
    let (out, _) = decode_all(&[
        "data: {\"id\":\"x\",\"choices\":[{\"index\":1,\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
    ]);
    assert!(!out.iter().any(|r| matches!(r, Output::Finish(_))));
    // The other choice is kept, in this format's words.
    assert!(
        matches!(&out[1..], [Output::Native(NativeValue { format, .. })] if &**format == OPENAI)
    );
}

#[test]
fn content_the_kind_cannot_hold_is_other() {
    let (out, _) = decode_all(&[
        "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"refusal\":\"no\"}}]}\n\n",
    ]);
    let refusal = json!({ "choices": [{ "index": 0, "delta": { "refusal": "no" } }] });
    assert_eq!(out[1..], [Output::native(OPENAI, refusal)]);
}

#[test]
fn a_chunk_is_all_it_says_as_events_in_order() {
    let (out, _) = decode_all(&[
        "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}],\"usage\":{\"completion_tokens\":1}}\n\n",
    ]);
    assert_eq!(
        out,
        [
            Output::start("x", ""),
            Output::usage(Usage {
                output_tokens: 1,
                ..Usage::default()
            }),
            Output::block_start(0, Block::Text),
            Output::text(0, "hi"),
            Output::block_end(0),
            Output::finished(Finish::Stop),
        ]
    );
}

// A decoder ends only at the end of its body, so what follows the
// terminator is read and ignored; a body that ends before it is cut
// short.
#[test]
fn the_terminator_completes_the_response_and_nothing_after_it_is_read() {
    let (out, end) = decode_all(&["data: [DONE]\n\ndata: {\"choices\":[]}\n\n"]);
    assert!(out.is_empty() && end.is_ok());
    let (_, end) = decode_all(&["data: {\"choices\":[]}\n\n"]);
    assert_eq!(
        end.err().map(|f| f.code().to_owned()).as_deref(),
        Some("truncated")
    );
}

// Some OpenAI-compatible servers end a finished stream without `[DONE]`:
// it is whole once its finish and usage came, but not before its usage,
// which the call asked for.
#[test]
fn a_stream_that_ends_well_after_its_finish_and_usage_is_whole_without_done() {
    let text = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n";
    let finish = "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
    let usage = "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n";
    let truncated = |end: Result<(), Fault>| end.err().is_some_and(|f| f.code() == "truncated");
    let (_, end) = decode_all(&[text, finish, usage]);
    assert!(end.is_ok(), "{end:?}");
    assert!(truncated(decode_all(&[text, finish]).1));
    assert!(truncated(decode_all(&[text, usage]).1));
}

#[test]
fn a_provider_error_event_ends_the_stream_with_its_code() {
    let (_, end) = decode_all(&[
        "data: {\"error\":{\"type\":\"server_error\",\"message\":\"overloaded\"}}\n\n",
    ]);
    assert!(matches!(
        end,
        Err(f) if f.code() == "server_error" && f.message() == "overloaded"
    ));
}

#[test]
fn a_provider_error_event_goes_out_as_it_came_in_the_same_format() {
    let error = "data: {\"error\":{\"message\":\"boom\",\"type\":\"server_error\",\"param\":null,\"code\":null}}\n\n";
    let (_, end) = decode_all(&[error]);
    let mut bytes = BytesMut::new();
    let mut enc = Chunks::new(false);
    assert!(enc.end_into(&end, &mut bytes).is_ok());
    // Its fields are the format's, so it is written as it came.
    assert_eq!(&bytes[..], error.as_bytes());
}

#[test]
fn an_edited_start_is_written_in_every_chunk() {
    let mut replies = vec![Output::start("y", "alias")];
    replies.extend(text_events(0, "HI"));
    let stream = stream_of(&replies, false).expect("a stream");
    let chunks = chunks_of(&stream);
    assert!(
        chunks
            .iter()
            .all(|c| c["id"] == "y" && c["model"] == "alias")
    );
    let text: String = chunks
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(text, "HI");
    // Text a filter replaced with a finish is the finish.
    let length = [Output::start("y", "m"), Output::finished(Finish::Length)];
    let stream = stream_of(&length, false).expect("a stream");
    assert!(stream.contains("\"finish_reason\":\"length\""), "{stream}");
}

#[test]
fn a_finish_edited_beside_a_tool_call_is_written() {
    let replies = [
        Output::block_start(0, call("t", "f")),
        Output::block_end(0),
        Output::finished(Finish::Stop),
    ];
    let stream = stream_of(&replies, false).expect("a stream");
    assert!(stream.contains("\"finish_reason\":\"stop\""), "{stream}");
    // A call that had no arguments has none, as JSON.
    assert!(stream.contains("\"arguments\":\"{}\""), "{stream}");
}

#[test]
fn a_tool_call_emptied_in_a_stream_fails_rather_than_leave_a_shell() {
    // Arguments for a call that never started.
    let replies = [Output::arguments(0, "{}")];
    assert!(stream_of(&replies, false).is_err());
    assert!(body_of(&replies).is_err());
    // A call that is a shell, with no id or name, cannot be decoded.
    let (_, end) = decode_all(&[
        "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0}]}}]}\n\n",
    ]);
    assert!(end.is_err());
}

#[test]
fn the_text_of_the_opening_chunk_can_be_cleared() {
    let (out, _) = decode_all(&[
        "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"SECRET\"}}]}\n\n",
    ]);
    let kept: Vec<Output> = out
        .into_iter()
        .filter(|r| !matches!(r, Output::Delta { .. }))
        .collect();
    let stream = stream_of(&kept, false).expect("a stream");
    assert!(!stream.contains("SECRET"), "{stream}");
}

#[test]
fn a_tool_call_a_filter_replaces_with_text_is_gone() {
    let mut replies = vec![Output::start("x", "m")];
    replies.extend(text_events(0, "[removed]"));
    let stream = stream_of(&replies, false).expect("a stream");
    assert!(stream.contains("\"content\":\"[removed]\""), "{stream}");
    assert!(!stream.contains("tool_calls"), "{stream}");
    // Another format's event is dropped.
    let foreign = [Output::native("anthropic-messages", json!({ "type": "x" }))];
    let body = body_of(&foreign).expect("a body");
    assert!(!body.to_string().contains("\"x\""), "{body}");
}

#[test]
fn a_whole_response_decodes_into_events() {
    let v = json!({
        "id": "c1", "model": "gpt", "created": 7,
        "choices": [{ "index": 0, "finish_reason": "tool_calls", "message": {
            "role": "assistant", "content": "ok", "refusal": null,
            "tool_calls": [{ "id": "t", "type": "function", "function": { "name": "f", "arguments": "{}" } }],
        }}],
        "usage": { "prompt_tokens": 2, "completion_tokens": 1 },
    });
    let mut start = Output::start("c1", "gpt");
    if let Output::Start(s) = &mut start {
        let fields = json!({ "created": 7 });
        s.native = Some(NativeFields::new(
            OPENAI,
            fields.as_object().cloned().unwrap_or_default(),
        ));
    }
    let mut totals = Output::usage(Usage {
        input_tokens: 2,
        ..Usage::default()
    });
    if let Output::Usage(u) = &mut totals {
        u.output_tokens += 1;
    }
    assert_eq!(
        decode_body(&v),
        [
            start,
            totals,
            Output::block_start(0, Block::Text),
            Output::text(0, "ok"),
            Output::block_end(0),
            Output::block_start(1, call("t", "f")),
            Output::arguments(1, "{}"),
            Output::block_end(1),
            Output::finished(Finish::ToolUse),
        ]
    );
    // Logprobs and content that is not text are other content too.
    let logprobs = json!({ "choices": [{ "index": 0, "logprobs": { "content": [] },
        "message": { "role": "assistant", "content": "ok" } }] });
    assert!(matches!(
        decode_body(&logprobs).first(),
        Some(Output::Native(NativeValue { .. }))
    ));
    let parts = json!({ "choices": [{ "index": 0,
        "message": { "role": "assistant", "content": [{ "type": "text", "text": "ok" }] } }] });
    assert!(matches!(
        decode_body(&parts).first(),
        Some(Output::Native(NativeValue { .. }))
    ));
    // A whole body collects what the events say, and keeps its other fields.
    let again = body_of(&decode_body(&v)).expect("a body");
    assert_eq!(again["created"], 7);
    let (said, sent) = (&again["choices"][0]["message"], &v["choices"][0]["message"]);
    assert_eq!(said["content"], "ok");
    assert_eq!(said["tool_calls"], sent["tool_calls"]);
    assert_eq!(again["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(again["usage"]["prompt_tokens"], 2);
}

#[test]
fn cache_writes_are_written_beside_cached_tokens_and_bad_usage_fails() {
    // Another provider's cache writes, as Responses names them.
    let mut written = vec![Output::start("x", "m")];
    written.extend(text_events(0, "ok"));
    written.push(Output::usage(Usage {
        input_tokens: 10,
        cached_input_tokens: 2,
        cache_write_input_tokens: 3,
        ..Usage::default()
    }));
    let body = body_of(&written).expect("written");
    assert_eq!(
        body["usage"]["prompt_tokens_details"],
        json!({ "cached_tokens": 2, "cache_write_tokens": 3 })
    );
    assert!(stream_of(&written, true).is_ok());
    // Read back as it was written.
    let read = decode_body(&json!({ "id": "x", "model": "m", "choices": [],
        "usage": { "prompt_tokens": 10, "completion_tokens": 0,
                   "prompt_tokens_details": { "cache_write_tokens": 3 } } }));
    assert!(
        read.iter()
            .any(|o| matches!(o, Output::Usage(u) if u.cache_write_input_tokens == 3)),
        "{read:?}"
    );
    // Cached tokens are part of the input: more than it is no usage.
    let more = [Output::usage(Usage {
        cached_input_tokens: 3,
        ..Usage::default()
    })];
    assert!(body_of(&more).is_err());
}

#[test]
fn what_a_later_chunk_changes_of_the_response_is_native_after_its_usage() {
    let events = [
        "data: {\"id\":\"g\",\"system_fingerprint\":\"fp\",\"x_vendor\":{\"id\":\"r\"},\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n",
        "data: {\"id\":\"g\",\"system_fingerprint\":\"fp\",\"x_vendor\":{\"id\":\"r\"},\"choices\":[{\"index\":0,\"delta\":{\"content\":\"!\"}}]}\n\n",
        "data: {\"id\":\"g\",\"system_fingerprint\":\"fp\",\"x_vendor\":{\"id\":\"r\",\"usage\":{\"prompt_tokens\":1}},\"choices\":[],\"usage\":{\"prompt_tokens\":1}}\n\n",
        "data: [DONE]\n\n",
    ];
    let (out, end) = decode_all(&events);
    assert!(end.is_ok());
    let Some(Output::Start(start)) = out.first() else {
        panic!("{out:?}");
    };
    assert_eq!(start.native.fields()["x_vendor"], json!({ "id": "r" }));
    // Fields said again as they were are no news.
    let changed =
        json!({ "response": { "x_vendor": { "id": "r", "usage": { "prompt_tokens": 1 } } } });
    let at = out
        .iter()
        .position(|r| *r == Output::native(OPENAI, changed.clone()));
    let usage = out.iter().position(|r| matches!(r, Output::Usage(_)));
    assert!(usage < at && at.is_some(), "{out:?}");
    assert_eq!(
        out.iter()
            .filter(|r| matches!(r, Output::Native(_)))
            .count(),
        1
    );
    // A client of this format gets them on the next chunk; another
    // drops them, as it drops the start's.
    let stream = stream_of(&out, false).expect("a stream");
    let chunks = chunks_of(&stream);
    let last = chunks.last().expect("the finish");
    assert_eq!(
        last["x_vendor"], changed["response"]["x_vendor"],
        "{stream}"
    );
    assert_eq!(chunks[0]["x_vendor"], json!({ "id": "r" }));
    assert_eq!(body_of(&out).expect("a body")["x_vendor"], last["x_vendor"]);
    let mut whole = Assembled::default();
    out.iter()
        .try_for_each(|r| whole.push(r))
        .expect("assembled");
    assert!(AnthropicMessages.encode_whole(&whole).is_ok());
}

#[test]
fn whole_usage_keeps_its_details() {
    {
        let u = json!({
            "prompt_tokens": 4, "completion_tokens": 7, "total_tokens": 11,
            "prompt_tokens_details": {"cached_tokens": 2, "audio_tokens": 1},
            "completion_tokens_details": {"reasoning_tokens": 3},
        });
        let mut v = json!({"id": "c", "model": "m", "choices": [{"index": 0,
            "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}]});
        v["usage"] = u;
        let events = decode_body(&v);
        let total = events.iter().fold(Usage::default(), |mut all, r| {
            if let Output::Usage(u) = r {
                all += u;
            }
            all
        });
        let expected = Usage {
            input_tokens: 4,
            output_tokens: 7,
            cached_input_tokens: 2,
            reasoning_tokens: 3,
            ..Usage::default()
        };
        assert_eq!(total, expected);
        let again = body_of(&events).expect("a body");
        let usage = &again["usage"];
        assert_eq!(usage["total_tokens"], 11);
        // What has no unit travels with the response.
        assert_eq!(
            usage["prompt_tokens_details"],
            json!({"cached_tokens": 2, "audio_tokens": 1})
        );
        assert_eq!(
            usage["completion_tokens_details"],
            json!({"reasoning_tokens": 3})
        );
    }
}

#[test]
fn a_usage_chunk_the_client_did_not_ask_for_is_not_written() {
    let first = "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3}}\n\n";
    // OpenAI repeats the totals in a usage chunk the client did not ask
    // for; its own fields, such as `obfuscation`, do not keep it.
    let again =
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3},\"obfuscation\":\"xyz\"}\n\n";
    let (replies, end) = decode_all(&[first, again, "data: [DONE]\n\n"]);
    assert!(end.is_ok());
    let counted: Vec<_> = replies
        .iter()
        .filter_map(|r| match r {
            Output::Usage(u) => Some(u.input_tokens).filter(|&n| n > 0),
            _ => None,
        })
        .collect();
    assert_eq!(counted, [3]);
    // It counts, but neither it nor the usage on a chunk with choices is
    // written.
    let out = stream_of(&replies, false).expect("a stream");
    assert!(!out.contains("usage") && out.contains("\"stop\""), "{out}");
    assert!(!out.contains("obfuscation"), "{out}");
    assert!(out.ends_with("data: [DONE]\n\n"));
}

#[test]
fn an_sse_envelope_is_no_part_of_a_response() {
    let plain = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"secret\"}}]}\n\n";
    let wrapped = "id: 7\nevent: completion.chunk\nretry: 1500\n: kept\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"secret\"}}]}\n\n";
    assert_eq!(decode_all(&[plain]).0, decode_all(&[wrapped]).0);
}

#[test]
fn stream_usage_is_cumulative_on_the_wire_and_written_as_one_total() {
    let events = [
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2,\"vendor\":9}}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":7,\"total_tokens\":8,\"vendor\":10}}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":7,\"total_tokens\":8,\"vendor\":11}}\n\n",
        "data: [DONE]\n\n",
    ];
    let (mut replies, _) = decode_all(&events);
    let outputs: Vec<_> = replies
        .iter()
        .filter_map(|r| match r {
            Output::Usage(u) => Some(u.output_tokens).filter(|&n| n > 0),
            _ => None,
        })
        .collect();
    assert_eq!(outputs, [1, 6]);
    replies.retain(|r| matches!(r, Output::Usage(_)));
    // A plugin lowers the second increment: the total follows it.
    replies[1] = Output::usage(Usage {
        output_tokens: 2,
        ..Usage::default()
    });
    let stream = stream_of(&replies, true).expect("a stream");
    let last = chunks_of(&stream).pop().unwrap_or_default();
    assert_eq!(last["usage"]["completion_tokens"], 3, "{stream}");
}

#[test]
fn dropped_usage_does_not_reappear_in_later_totals() {
    {
        let events: Vec<_> = [1, 7, 7]
            .map(|n| {
                let usage = json!({"completion_tokens": n, "vendor": 9});
                let event = json!({"choices": [], "usage": usage});
                format!("data: {event}\n\n")
            })
            .into();
        let (mut replies, _) =
            decode_all(&[&events[0], &events[1], &events[2], "data: [DONE]\n\n"]);
        // The second report, which adds 6, is dropped.
        replies.retain(|r| !matches!(r, Output::Usage(u) if u.output_tokens == 6));
        let stream = stream_of(&replies, true).expect("a stream");
        let again = decode_all(&[&stream]).0;
        let total: u64 = again
            .iter()
            .filter_map(|r| match r {
                Output::Usage(u) => Some(u.output_tokens).filter(|&n| n > 0),
                _ => None,
            })
            .sum();
        assert_eq!(total, 1, "{stream}");
    }
}

#[test]
fn usage_never_decreases_and_edits_upward_are_summed() {
    for (totals, edited, expected) in [([7, 1, 0], None, 7), ([1, 7, 7], Some(3), 9)] {
        let events = totals.map(|n| {
            format!("data: {{\"choices\":[],\"usage\":{{\"completion_tokens\":{n}}}}}\n\n")
        });
        let (mut replies, _) =
            decode_all(&[&events[0], &events[1], &events[2], "data: [DONE]\n\n"]);
        if let Some(amount) = edited {
            replies[0] = Output::usage(Usage {
                output_tokens: amount,
                ..Usage::default()
            });
        }
        let stream = stream_of(&replies, true).expect("a stream");
        let last = chunks_of(&stream).pop().unwrap_or_default();
        assert_eq!(last["usage"]["completion_tokens"], expected, "{stream}");
    }
}

/// The request `body` with what the format adds ([`Schema::finish`]).
fn with_usage(body: &str) -> Value {
    let mut v: Map<String, Value> = serde_json::from_str(body).unwrap_or_default();
    OpenAiChat::prepare_call(&mut v);
    v.into()
}

#[test]
fn a_request_that_needs_nothing_goes_as_it_came() {
    let body = r#"{"model":"a","stream":false}"#;
    assert_eq!(
        with_usage(body),
        serde_json::from_str::<Value>(body).unwrap_or_default()
    );
}

#[test]
fn streaming_requests_ask_for_usage() {
    assert_eq!(
        with_usage(r#"{"model":"a","stream":true}"#),
        json!({"model":"a","stream":true,"stream_options":{"include_usage":true}})
    );
}

#[test]
fn null_stream_options_still_ask_for_usage() {
    let body = r#"{"model":"a","stream":true,"stream_options":null}"#;
    assert_eq!(
        with_usage(body),
        json!({"model":"a","stream":true,"stream_options":{"include_usage":true}})
    );
}

#[test]
fn asking_for_usage_keeps_the_clients_other_stream_options() {
    let body = r#"{"model":"a","stream":true,"stream_options":{"include_obfuscation":false}}"#;
    assert_eq!(
        with_usage(body),
        json!({"model":"a","stream":true,"stream_options":{"include_obfuscation":false,"include_usage":true}})
    );
}

/// The client's `body` as the conversation it says, with `edit` made,
/// written as a request.
fn written(body: Value, edit: impl FnOnce(&mut Input)) -> Result<Value, Fault> {
    let mut chat = OpenAiChat.decode_request(Bytes::from(body.to_string()))?;
    edit(&mut chat);
    Ok(OpenAiChat.encode_request(&chat)?.into())
}

#[test]
fn a_request_round_trips_through_a_conversation() {
    let body = json!({
        "model": "m",
        "messages": [
            { "role": "system", "content": "be brief" },
            { "role": "user", "content": [{ "type": "text", "text": "hi" }] },
            { "role": "developer", "content": "and kind" },
        ],
        "max_tokens": 5,
        "stop": "x",
        "n": 1,
    });
    assert_eq!(
        written(body, |_| {}).ok(),
        Some(json!({
            "model": "m",
            "messages": [
                { "role": "system", "content": "be brief" },
                { "role": "user", "content": "hi" },
                { "role": "developer", "content": "and kind" },
            ],
            "max_completion_tokens": 5,
            "stop": ["x"],
        }))
    );
}

/// Hides an email in tool calls and results, as a PII plugin does.
fn hide_email(chat: &mut Input) {
    for part in chat.messages.iter_mut().flat_map(|m| &mut m.content) {
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
}

#[test]
fn an_edited_tool_call_and_result_are_written_into_the_request() {
    let call = json!({ "id": "c1", "type": "function",
        "function": { "name": "lookup", "arguments": "{\"email\":\"bob@x.io\"}" } });
    let tool = json!({ "type": "function", "function": { "name": "lookup", "strict": true,
        "parameters": { "type": "object" } } });
    let body = json!({ "model": "m", "tools": [tool], "tool_choice": "required",
    "messages": [
        { "role": "user", "content": "find bob" },
        { "role": "assistant", "content": null, "tool_calls": [call] },
        { "role": "tool", "tool_call_id": "c1", "content": "bob@x.io is Bob" },
    ]});
    let chat = OpenAiChat.decode_json(&body);
    assert_eq!(
        chat.messages[1].content,
        [Content::tool_call(
            "c1",
            "lookup",
            "{\"email\":\"bob@x.io\"}"
        )]
    );
    assert_eq!(chat.options.tool_choice, Some(ToolChoice::Required));
    assert_eq!(
        chat.tools[0].native.fields().get("strict"),
        Some(&json!(true))
    );
    assert_eq!(chat.tools[0].native.format(), Some(OPENAI));
    assert!(chat.native.is_empty());
    assert_eq!(written(body.clone(), |_| {}).ok(), Some(body.clone()));
    let mut expected = body.clone();
    expected["messages"][1]["tool_calls"][0]["function"]["arguments"] =
        "{\"email\":\"<EMAIL_1>\"}".into();
    expected["messages"][2]["content"] = "<EMAIL_1> is Bob".into();
    assert_eq!(written(body, hide_email).ok(), Some(expected));
}

#[test]
fn tool_results_and_calls_are_written_as_openai_messages() {
    let mut both = Message::new(Role::User, "and then?");
    both.content
        .insert(0, Content::tool_result("c1", vec!["42".into()], false));
    let mut asked = Message::new(Role::Assistant, "");
    asked.content = vec![Content::tool_call("c1", "f", "{}")];
    let chat = Input::new([asked, both]);
    let v = OpenAiChat.encode_request(&chat).map(Value::Object);
    assert_eq!(
        v.ok().map(|v| v["messages"].clone()),
        Some(json!([
            { "role": "assistant", "content": null, "tool_calls": [
                { "id": "c1", "type": "function", "function": { "name": "f", "arguments": "{}" } },
            ] },
            { "role": "tool", "tool_call_id": "c1", "content": "42" },
            { "role": "user", "content": "and then?" },
        ]))
    );
    // OpenAI's tool messages hold text alone.
    let image = Content::Image(Media::new("image/png", MediaSource::Url("u".into())));
    let mut shown = Message::new(Role::User, "");
    shown.content = vec![Content::tool_result("c1", vec![image], false)];
    assert!(OpenAiChat.encode_request(&Input::new([shown])).is_err());
    // OpenAI cannot say a call failed.
    let mut failed = Message::new(Role::User, "");
    failed.content = vec![Content::tool_result("c1", vec!["no".into()], true)];
    assert!(OpenAiChat.encode_request(&Input::new([failed])).is_err());
}

#[test]
fn what_a_conversation_cannot_hold_is_kept_or_refused() {
    let body = json!({ "model": "m", "messages": [
        { "role": "user", "name": "alice", "content": "hi" },
    ], "tools": [{ "type": "function", "function": { "name": "f" } }], "seed": 7 });
    let chat = OpenAiChat.decode_json(&body);
    assert_eq!(chat.native.fields().get("seed"), Some(&json!(7)));
    assert_eq!(chat.native.format(), Some(OPENAI));
    // A function tool is the kind's.
    assert_eq!(chat.tools, [Tool::new("f", Value::Null)]);
    assert_eq!(chat.messages[0].text(), "hi");
    assert_eq!(
        chat.messages[0].native.fields().get("name"),
        Some(&json!("alice"))
    );
    // To another format it is refused; to OpenAI it is kept.
    let err = AnthropicMessages.encode_request(&chat).err();
    assert_eq!(
        err.map(|e| respan_gateway_http_faults::status(&e).as_u16()),
        Some(400)
    );
    assert_eq!(written(body.clone(), |_| {}).ok(), Some(body));
    // With the native fields cleared and only plain messages, it is sent.
    let mut chat = chat;
    chat.native = None;
    chat.messages = vec![Message::new(Role::User, "hi")];
    assert!(AnthropicMessages.encode_request(&chat).is_ok());
}

#[test]
fn several_choices_are_refused_as_a_bad_request() {
    let body = json!({ "model": "m", "messages": [], "n": 2 });
    let chat = OpenAiChat.decode_json(&body);
    assert_eq!(chat.native.fields().get("n"), Some(&json!(2)));
    let refused = OpenAiChat.encode_request(&chat).err();
    assert!(refused.is_some_and(|f| f.is(respan_gateway_core::FaultKind::Client)));
    // One choice is the default, and says nothing.
    let one = OpenAiChat.decode_json(&json!({ "model": "m", "messages": [], "n": 1 }));
    assert!(one.native.is_empty());
}

#[test]
fn the_user_field_is_read_redacted_and_translated() {
    let body = json!({ "model": "m", "user": "bob@x.io",
        "messages": [{ "role": "user", "content": "hi" }] });
    let chat = OpenAiChat.decode_json(&body);
    assert_eq!(chat.user.as_deref(), Some("bob@x.io"));
    assert!(chat.native.is_empty());
    // Kept as it came, or changed or removed by a plugin.
    assert_eq!(written(body.clone(), |_| {}).ok(), Some(body.clone()));
    let redacted = written(body.clone(), |c| c.user = Some("[x]".into()));
    assert_eq!(redacted.ok().map(|v| v["user"].clone()), Some(json!("[x]")));
    let removed = written(body.clone(), |c| c.user = None);
    assert_eq!(
        removed.ok(),
        Some(json!({ "model": "m", "messages": [{ "role": "user", "content": "hi" }] }))
    );
    // To Anthropic it is the metadata's user id, and back.
    let sent = AnthropicMessages.encode_request(&chat).map(Value::from);
    let sent = sent.unwrap_or_default();
    assert_eq!(sent["metadata"], json!({ "user_id": "bob@x.io" }));
    let back = OpenAiChat.encode_request(&AnthropicMessages.decode_json(&sent));
    assert_eq!(
        back.ok().map(|v| v["user"].clone()),
        Some(json!("bob@x.io"))
    );
    // A user that is not a string is not the kind's: it stays in `native`.
    let odd = OpenAiChat.decode_json(&json!({ "user": 7, "messages": [] }));
    assert_eq!(
        (odd.user, odd.native.fields().get("user")),
        (None, Some(&json!(7)))
    );
}

#[test]
fn a_plugin_that_removes_tools_sends_the_request_without_them() {
    let body = json!({ "model": "m", "messages": [], "tools": [{ "type": "function" }] });
    let edited = written(body, |c| {
        c.native.fields_mut(OPENAI).shift_remove("tools");
        c.native.fields_mut(OPENAI).insert("seed".into(), 7.into());
    });
    assert_eq!(
        edited.ok(),
        Some(json!({ "model": "m", "messages": [], "seed": 7 }))
    );
}

#[test]
fn the_limit_is_read_as_the_larger_spelling_and_written_canonically() {
    let v = json!({ "max_tokens": 100_000, "max_completion_tokens": 10, "messages": [] });
    assert_eq!(
        OpenAiChat.decode_json(&v).options.max_output_tokens,
        Some(100_000)
    );
    for (limits, want) in [
        (
            json!({ "max_tokens": 4000 }),
            json!({ "max_completion_tokens": 10 }),
        ),
        (
            json!({ "max_completion_tokens": 4000 }),
            json!({ "max_completion_tokens": 10 }),
        ),
        (
            json!({ "max_tokens": 4000, "max_completion_tokens": 4000 }),
            json!({ "max_completion_tokens": 10 }),
        ),
        (json!({}), json!({ "max_completion_tokens": 10 })),
    ] {
        let mut body = json!({ "model": "m", "messages": [] });
        if let Some(fields) = body.as_object_mut() {
            fields.extend(limits.as_object().cloned().unwrap_or_default());
        }
        let edited =
            written(body.clone(), |c| c.options.max_output_tokens = Some(10)).unwrap_or_default();
        for key in ["max_tokens", "max_completion_tokens"] {
            assert_eq!(edited.get(key), want.get(key), "{body}: {edited}");
        }
        // Removed, it is gone under every spelling.
        let none = written(body, |c| c.options.max_output_tokens = None).unwrap_or_default();
        assert!(none.get("max_tokens").is_none() && none.get("max_completion_tokens").is_none());
    }
    // Another format writes its own spelling.
    let chat = OpenAiChat
        .decode_json(&json!({ "max_tokens": 7, "messages": [{"role":"user","content":"hi"}] }));
    let sent = AnthropicMessages.encode_request(&chat).map(Value::from);
    assert_eq!(sent.map(|v| v["max_tokens"].clone()).ok(), Some(json!(7)));
}

#[test]
fn whether_it_streams_is_written_with_its_options() {
    let body = json!({ "model": "m", "stream": true, "stream_options": { "include_usage": true },
        "messages": [{ "role": "user", "content": "hi" }] });
    let out = written(body, |_| {}).unwrap_or_default();
    assert!(out.get("stream").is_none());
    assert!(out.get("stream_options").is_none());
}

#[test]
fn a_removed_field_is_not_written_and_the_others_are() {
    let body = json!({ "model": "m", "temperature": 0.5, "messages": [], "top_p": 0.9, "seed": 1 });
    let out = written(body, |c| c.options.temperature = None).unwrap_or_default();
    assert_eq!(
        out,
        json!({ "model": "m", "messages": [], "top_p": 0.9, "seed": 1 })
    );
}

#[test]
fn keys_set_to_null_say_nothing() {
    let body = json!({ "model": "m", "tools": null, "n": null, "messages": [
        { "role": "user", "name": null, "content": "hi" },
        { "role": "assistant", "content": "yo", "refusal": null },
    ]});
    let chat = OpenAiChat.decode_json(&body);
    assert!(chat.native.is_empty(), "{:?}", chat.native);
    assert_eq!(chat.messages.len(), 2);
    assert!(chat.messages.iter().all(Message::is_plain));
}

#[test]
fn an_edited_message_is_written_with_its_role_and_its_other_fields() {
    let body = json!({ "model": "m", "messages": [
        { "role": "developer", "content": "key sk-1", "name": "ops" },
    ]});
    let edited = written(body, |c| c.messages[0].content = vec!["key [x]".into()]);
    // The message keeps its spelling of the role, and its name.
    assert_eq!(
        edited.map(|v| v["messages"].clone()).ok(),
        Some(json!([{ "role": "developer", "content": "key [x]", "name": "ops" }]))
    );
}

#[test]
fn a_system_prompt_can_be_added_to_a_conversation_with_tool_calls() {
    let call = json!({ "role": "assistant", "content": null, "tool_calls": [{ "id": "t", "type": "function", "function": { "name": "f", "arguments": "{}" } }] });
    let result = json!({ "role": "tool", "tool_call_id": "t", "content": "42" });
    let body = json!({ "model": "m", "messages": [
        { "role": "user", "content": "hi" }, call.clone(), result.clone(),
        { "role": "user", "content": "thanks" },
    ]});
    let edited = written(body, |c| {
        c.messages.insert(0, Message::new(Role::System, "be kind"))
    });
    assert_eq!(
        edited.map(|v| v["messages"].clone()).ok(),
        Some(json!([
            { "role": "system", "content": "be kind" },
            { "role": "user", "content": "hi" }, call, result,
            { "role": "user", "content": "thanks" },
        ]))
    );
}

#[test]
fn what_a_message_cannot_hold_is_kept_where_it_is() {
    let image = json!({ "type": "image_url", "image_url": { "url": "u" } });
    let tool = json!({ "role": "tool", "tool_call_id": "t", "content": "42" });
    let body = json!({ "model": "m", "messages": [
        { "role": "user", "content": [{ "type": "text", "text": "what is this?" }, image] },
        tool,
    ]});
    let chat = OpenAiChat.decode_json(&body);
    assert_eq!(chat.messages[0].text(), "what is this?");
    // A tool message is a user message of one result.
    assert_eq!(chat.messages[1].role, Role::User);
    assert_eq!(
        chat.messages[1].content,
        [Content::tool_result("t", vec!["42".into()], false)]
    );
    assert!(chat.messages[1].native.is_empty());
    // The text is edited in its part, beside the image and tool result.
    let edited = written(body, |c| {
        c.messages[0].content[0] = "WHAT IS THIS?".into();
    });
    assert_eq!(
        edited.map(|v| v["messages"].clone()).ok(),
        Some(json!([
            { "role": "user", "content": [{ "type": "text", "text": "WHAT IS THIS?" }, image] },
            tool,
        ]))
    );
}

#[test]
fn a_long_conversation_changed_throughout_is_written_message_by_message() {
    let messages: Vec<Value> = (0..5000)
        .map(|i| json!({ "role": "user", "content": format!("m{i}") }))
        .collect();
    let body = json!({ "model": "m", "messages": messages });
    let edited = written(body, |c| {
        c.messages.insert(0, Message::new(Role::System, "be kind"));
        for m in &mut c.messages[1..] {
            m.content = vec![m.text().to_uppercase().into()];
        }
    });
    let messages = edited.map(|v| v["messages"].clone()).unwrap_or_default();
    assert_eq!(
        messages[0],
        json!({ "role": "system", "content": "be kind" })
    );
    assert_eq!(
        messages[5000],
        json!({ "role": "user", "content": "M4999" })
    );
}

#[test]
fn an_insert_and_an_edit_elsewhere_keep_the_messages_between() {
    // Claude Code marks cache breakpoints on earlier messages: a part
    // with a marker is the format's own, and is written as it is.
    let cached = json!({ "role": "user", "content": [
        { "type": "text", "text": "the repo", "cache_control": { "type": "ephemeral" } },
    ]});
    let body = json!({ "model": "m", "messages": [
        cached,
        { "role": "assistant", "content": "ok" },
        { "role": "user", "content": "my SSN is 123" },
    ]});
    let edited = written(body, |c| {
        c.messages.insert(0, Message::new(Role::System, "be kind"));
        c.messages[3].content = vec!["my SSN is [redacted]".into()];
    });
    assert_eq!(
        edited.map(|v| v["messages"].clone()).ok(),
        Some(json!([
            { "role": "system", "content": "be kind" },
            cached,
            { "role": "assistant", "content": "ok" },
            { "role": "user", "content": "my SSN is [redacted]" },
        ]))
    );
}

#[test]
fn messages_are_added_where_the_plugin_puts_them_among_images() {
    let image = json!({ "role": "user", "content": [{ "type": "image_url", "image_url": { "url": "u" } }] });
    let body = json!({ "model": "m", "messages": [image] });
    let added = written(body, |c| {
        c.messages.insert(0, Message::new(Role::System, "be kind"));
        c.messages.push(Message::new(Role::User, "and"));
    });
    assert_eq!(
        added.map(|v| v["messages"].clone()).ok(),
        Some(json!([
            { "role": "system", "content": "be kind" },
            image,
            { "role": "user", "content": "and" },
        ]))
    );
}

#[test]
fn a_message_added_to_a_request_with_none_does_not_panic() {
    let edited = written(json!({ "model": "m" }), |c| {
        c.messages.insert(0, Message::new(Role::System, "be kind"));
    });
    assert_eq!(
        edited.map(|v| v["messages"].clone()).ok(),
        Some(json!([{ "role": "system", "content": "be kind" }]))
    );
}

#[test]
fn an_edit_changes_only_what_the_edit_changed() {
    let body = json!({
        "model": "m", "temperature": 0.5, "top_p": 0.25, "stop": ["x"],
        "messages": [{ "role": "user", "content": "hi" }],
    });
    let edited = written(body.clone(), |c| {
        c.messages[0] = Message::new(Role::User, "HI")
    });
    let mut expected = body;
    expected["messages"][0]["content"] = "HI".into();
    assert_eq!(edited.ok(), Some(expected));
}

#[test]
fn an_edit_keeps_messages_a_conversation_cannot_hold() {
    let image = json!({ "role": "user", "content": [{ "type": "image_url", "image_url": { "url": "u", "detail": "high" } }] });
    let body = json!({ "model": "m", "messages": [
        { "role": "user", "content": "hi" }, image.clone(),
    ]});
    let edited = written(body.clone(), |c| {
        c.messages[0] = Message::new(Role::User, "HI")
    });
    assert_eq!(
        edited.map(|v| v["messages"].clone()).ok(),
        Some(json!([{ "role": "user", "content": "HI" }, image]))
    );
    // Removing it removes it; sending it in another format is refused.
    let removed = written(body.clone(), |c| drop(c.messages.pop()));
    assert_eq!(
        removed.map(|v| v["messages"].clone()).ok(),
        Some(json!([{ "role": "user", "content": "hi" }]))
    );
    let chat = OpenAiChat.decode_json(&body);
    assert!(AnthropicMessages.encode_request(&chat).is_err());
    assert!(OpenAiChat.encode_request(&chat).is_ok());
}

#[test]
fn what_is_already_in_the_targets_format_crosses_and_the_rest_is_refused() {
    let pdf = json!({ "type": "document", "source": { "type": "base64",
        "media_type": "application/pdf", "data": "JVBERi0=" } });
    let mut chat = OpenAiChat.decode_json(&json!({ "model": "m", "messages": [
        { "role": "user", "content": [{ "type": "text", "text": "read" },
            { "type": "file", "file": { "file_data": "x" } }] }] }));
    // A part in OpenAI's shape does not reach Anthropic.
    assert!(AnthropicMessages.encode_request(&chat).is_err());
    // One rewritten into Anthropic's does, as it is.
    chat.messages[0].content[1] = Content::native("anthropic-messages", pdf.clone());
    let sent = AnthropicMessages.encode_request(&chat).map(Value::from);
    assert_eq!(
        sent.map(|v| v["messages"][0]["content"][1].clone()).ok(),
        Some(pdf)
    );
    // The same holds for the fields of a request.
    let thinking = json!({ "thinking": { "type": "enabled" } });
    let fields = thinking.as_object().cloned().unwrap_or_default();
    chat.native = Some(NativeFields::new(OPENAI, fields.clone()));
    assert!(AnthropicMessages.encode_request(&chat).is_err());
    chat.native = Some(NativeFields::new("anthropic-messages", fields));
    let sent = AnthropicMessages.encode_request(&chat).map(Value::from);
    assert_eq!(
        sent.map(|v| v["thinking"].clone()).ok(),
        Some(json!({ "type": "enabled" }))
    );
}

/// Fields tagged as another format's are refused in a request, as a
/// message's and a tool's are.
#[test]
fn fields_in_another_format_are_not_written_into_a_request() {
    let body = json!({ "model": "m", "messages": [{ "role": "user", "content": "hi" }],
        "tools": [{ "type": "function", "function": { "name": "f" } }] });
    let chat = OpenAiChat.decode_json(&body);
    let thinking = json!({ "thinking": { "type": "enabled" } });
    let fields = thinking.as_object().cloned().unwrap_or_default();
    let mut foreign = chat.clone();
    foreign.native = Some(NativeFields::new("anthropic-messages", fields.clone()));
    let mut message = chat.clone();
    message.messages[0].native = Some(NativeFields::new("anthropic-messages", fields.clone()));
    let mut tool = chat.clone();
    tool.tools[0].native = Some(NativeFields::new("anthropic-messages", fields.clone()));
    let mut role = chat.clone();
    role.messages[0].role = Role::native("anthropic-messages", "critic");
    for chat in [foreign, message, tool, role] {
        let sent = OpenAiChat.encode_request(&chat);
        assert!(sent.is_err(), "sent: {:?}", sent.map(Value::from));
    }
    // Tagged for this format, the same is written.
    let mut own = chat;
    own.native = Some(NativeFields::new(OPENAI, fields));
    let sent = OpenAiChat.encode_request(&own).map(Value::from);
    assert_eq!(
        sent.map(|v| v["thinking"].clone()).ok(),
        Some(json!({ "type": "enabled" }))
    );
}

#[test]
fn a_stream_made_from_events_always_finishes() {
    let mut replies = vec![Output::start("x", "m")];
    replies.extend(text_events(0, "hi"));
    let out = stream_of(&replies, false).expect("a stream");
    assert!(out.contains("\"finish_reason\":\"stop\""), "{out}");
    assert!(out.ends_with("data: [DONE]\n\n"), "{out}");
}

#[test]
fn another_formats_events_are_dropped() {
    // What only a client of that format could read, a block's start or a
    // keep-alive alike.
    for kind in ["content_block_start", "ping"] {
        let other = Output::native("anthropic-messages", json!({ "type": kind }));
        let stream = stream_of(&[other], false).expect("a stream");
        assert!(!stream.contains(kind), "{stream}");
    }
}

// What a chunk has no place for is written beside the text.
#[test]
fn text_a_chunk_has_no_place_for_is_written_beside_it() {
    let (replies, _) = decode_all(&[
        "data: {\"id\":\"x\",\"choices\":[{\"index\":1,\"delta\":{\"content\":\"B\"}}]}\n\n",
    ]);
    let mut edited = replies;
    edited.extend(text_events(0, "note "));
    let out = stream_of(&edited, true).expect("a stream");
    assert!(
        out.contains("\"content\":\"note \"") && out.contains("\"index\":1"),
        "{out}"
    );
}

#[test]
fn a_whole_body_joins_every_text_and_keeps_calls() {
    let mut replies = vec![
        Output::start("x", "m"),
        Output::block_start(1, Block::Text),
        Output::text(1, "one "),
        Output::block_end(1),
        Output::block_start(2, Block::Text),
        Output::text(2, "two"),
        Output::block_end(2),
        Output::block_start(3, call("t", "f")),
        Output::block_end(3),
    ];
    let body = body_of(&replies).expect("a body");
    let message = &body["choices"][0]["message"];
    assert_eq!(message["content"], "one two");
    // A call with no arguments has none, as JSON.
    assert_eq!(message["tool_calls"][0]["function"]["arguments"], "{}");
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
    // Reasoning is its `reasoning_content`.
    replies.extend([
        Output::block_start(0, Block::Reasoning),
        Output::reasoning(0, "hm"),
        Output::block_end(0),
    ]);
    let body = body_of(&replies).expect("a body");
    assert_eq!(body["choices"][0]["message"]["reasoning_content"], "hm");
    let stream = stream_of(&replies, false).expect("a stream");
    assert!(stream.contains("\"reasoning_content\":\"hm\""), "{stream}");
}

#[test]
fn streamed_refusals_and_logprobs_join_in_a_whole_body() {
    let (out, end) = decode_all(&[
        "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":null,\"refusal\":\"I'm sorry, \"},\"logprobs\":{\"content\":null,\"refusal\":[{\"token\":\"a\"}]}}]}\n\n",
        "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"refusal\":\"I can't help.\"},\"logprobs\":{\"content\":null,\"refusal\":[{\"token\":\"b\"}]}}]}\n\n",
        "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    ]);
    assert!(end.is_ok());
    let body = body_of(&out).expect("a body");
    let choice = &body["choices"][0];
    assert_eq!(choice["message"]["refusal"], "I'm sorry, I can't help.");
    assert_eq!(choice["message"]["content"], Value::Null);
    let tokens = json!([{ "token": "a" }, { "token": "b" }]);
    assert_eq!(choice["logprobs"]["refusal"], tokens);
}

#[test]
fn reasoning_in_a_request_is_refused_and_anthropics_thinking() {
    let mut chat = OpenAiChat.decode_json(&json!({ "model": "m", "messages": [
        { "role": "user", "content": "hi" },
        { "role": "assistant", "content": "ok" },
    ]}));
    chat.messages[1].content.insert(
        0,
        Content::Reasoning {
            text: "hm".into(),
            native: None,
        },
    );
    // OpenAI Chat has no field for it; Anthropic, which refuses thinking it
    // did not sign, leaves it out of an earlier turn.
    assert!(OpenAiChat.encode_request(&chat).is_err());
    let sent = AnthropicMessages.encode_request(&chat).map(Value::from);
    assert_eq!(
        sent.ok().map(|v| v["messages"][1]["content"].clone()),
        Some(json!("ok"))
    );
    // Signed, it is Anthropic's thinking.
    let mut signature = serde_json::Map::new();
    signature.insert("signature".into(), "sig".into());
    chat.messages[1].content[0] = Content::Reasoning {
        text: "hm".into(),
        native: Some(NativeFields::new(AnthropicMessages.name(), signature)),
    };
    let sent = AnthropicMessages.encode_request(&chat).map(Value::from);
    let thinking = json!({ "type": "thinking", "thinking": "hm", "signature": "sig" });
    assert_eq!(
        sent.ok().map(|v| v["messages"][1]["content"][0].clone()),
        Some(thinking)
    );
}

/// A block is any number unique in the response, such as a plugin's
/// own: chunks number calls in the order they open, and a whole body
/// joins blocks in the order they open.
#[test]
fn blocks_of_any_numbers_are_written_in_the_order_they_open() {
    let far = u32::MAX - 1;
    let replies = [
        Output::start("x", "m"),
        Output::block_start(far, Block::Text),
        Output::text(far, "one "),
        Output::block_end(far),
        Output::block_start(7, call("t", "f")),
        Output::block_start(2, Block::Text),
        Output::text(2, "two"),
        Output::arguments(7, "{}"),
        Output::block_end(2),
        Output::block_start(u32::MAX, call("u", "g")),
        Output::block_end(7),
        Output::block_end(u32::MAX),
    ];
    let stream = stream_of(&replies, false).expect("a stream");
    let deltas: Vec<Value> = chunks_of(&stream)
        .iter()
        .map(|c| c["choices"][0]["delta"].clone())
        .collect();
    assert_eq!(deltas[1], json!({ "content": "one " }));
    assert_eq!(deltas[2]["tool_calls"][0]["index"], 0);
    assert_eq!(deltas[4]["tool_calls"][0]["index"], 0);
    assert_eq!(deltas[5]["tool_calls"][0]["index"], 1);
    let body = body_of(&replies).expect("a body");
    let message = &body["choices"][0]["message"];
    assert_eq!(message["content"], "one two");
    assert_eq!(message["tool_calls"][1]["id"], "u");
}

#[test]
fn the_fault_says_message_and_type_the_provider_the_rest() {
    let details = json!({"message": "", "type": "https://x/p", "code": null, "native": 1});
    let fault =
        Fault::bad_request("m").with(Details(details.as_object().expect("error fields").clone()));
    let body = OpenAiChat::error_body(&fault);
    // The fault's words are the format's; the provider's `code` stays.
    let kept =
        json!({"message": "m", "type": "bad_request", "param": null, "code": null, "native": 1});
    assert_eq!(body["error"], kept);
}

#[test]
fn a_plugins_fault_takes_one_shape_on_every_path() {
    let details = json!({"message": "x", "type": 5, "code": 7, "param": 3, "category": "pii"});
    let fault = Fault::new("pii")
        .with(ErrorCode("blocked".into()))
        .with(Status(StatusCode::FORBIDDEN))
        .with(Details(details.as_object().expect("error fields").clone()));
    let body = OpenAiChat::error_body(&fault);
    // OpenAI's `code` and `param` are strings: a number is not kept.
    let kept = json!({"message": "pii", "type": "blocked", "param": null, "code": "blocked", "category": "pii"});
    assert_eq!(body["error"], kept);
    // A string, as OpenAI's own errors give, is kept.
    let details = json!({"param": "messages", "code": "context_length_exceeded"});
    let fault = Fault::bad_request("too long")
        .with(Details(details.as_object().expect("error fields").clone()));
    let error = &OpenAiChat::error_body(&fault)["error"];
    assert_eq!(
        (&error["param"], &error["code"]),
        (&json!("messages"), &json!("context_length_exceeded"))
    );
}

#[test]
fn stop_after_a_tool_call_finishes_as_tool_use() {
    // A call forced by `tool_choice` finishes as "stop" at OpenAI.
    let body = json!({
        "id": "c1", "object": "chat.completion", "model": "m",
        "choices": [{ "index": 0, "finish_reason": "stop", "message": {
            "role": "assistant", "content": null,
            "tool_calls": [{ "id": "call_1", "type": "function",
                "function": { "name": "echo", "arguments": "{}" } }] } }],
    });
    let finish = |out: Vec<Output>| {
        out.into_iter().find_map(|o| match o {
            Output::Finish(f) => Some(f),
            _ => None,
        })
    };
    assert_eq!(finish(decode_body(&body)), Some(Finish::ToolUse));

    let chunk = |delta: Value, reason: Value| {
        let c = json!({ "id": "c1", "model": "m", "choices": [
            { "index": 0, "delta": delta, "finish_reason": reason }] });
        format!("data: {c}\n\n")
    };
    let call = json!({ "tool_calls": [{ "index": 0, "id": "call_1", "type": "function",
        "function": { "name": "echo", "arguments": "{}" } }] });
    let (out, end) = decode_all(&[
        &chunk(call, Value::Null),
        &chunk(json!({}), "stop".into()),
        "data: [DONE]\n\n",
    ]);
    assert!(end.is_ok());
    assert_eq!(finish(out), Some(Finish::ToolUse));

    // Without a call, "stop" stays a stop.
    let (out, _) = decode_all(&[
        &chunk(json!({ "content": "hi" }), "stop".into()),
        "data: [DONE]\n\n",
    ]);
    assert_eq!(finish(out), Some(Finish::Stop));
}

/// Regression: a tool call's own fields, such as a vendor's signature of it,
/// were lost when the call was replayed, as a tool loop's next step does.
#[test]
fn a_tool_calls_own_fields_are_kept_when_it_is_replayed() {
    let v = json!({ "id": "c", "model": "m", "choices": [{ "index": 0, "finish_reason": "tool_calls",
        "message": { "role": "assistant", "content": null, "tool_calls": [{
            "id": "call_1", "type": "function", "extra_content": { "google": { "thought_signature": "s" } },
            "function": { "name": "f", "arguments": "{}" } }] } }] });
    let mut whole = Assembled::default();
    for event in decode_body(&v) {
        whole.push(&event).expect("folds");
    }
    let replayed = whole.replay();
    let native = replayed.iter().find_map(|part| match part {
        Content::ToolCall { native, .. } => native.clone(),
        _ => None,
    });
    let native = native.expect("the call's fields");
    assert_eq!(&*native.format, OPENAI);
    assert_eq!(
        native.fields["extra_content"],
        json!({ "google": { "thought_signature": "s" } })
    );
    // Written again for this format, the call carries them.
    let mut message = Message::new(respan_gateway_conversation::Role::Assistant, "");
    message.content = replayed;
    let request = Input::new([
        Message::new(respan_gateway_conversation::Role::User, "hi"),
        message,
    ]);
    let body = OpenAiChat.encode_request(&request).expect("a request");
    let call = &body["messages"][1]["tool_calls"][0];
    assert_eq!(
        call["extra_content"]["google"]["thought_signature"], "s",
        "{body:?}"
    );
}
