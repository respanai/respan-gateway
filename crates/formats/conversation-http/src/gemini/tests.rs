use super::*;
use respan_gateway_core::Fault;

use super::encode::reason_of;
use super::request::is_synthetic;
use respan_gateway_conversation::Delta;
use respan_gateway_http::Decoder;
use respan_gateway_http::bytes::{Bytes, BytesMut};

use respan_gateway_conversation::{Content, Role, ToolChoice};

/// The responses of `chunks`, each decoded in turn.
fn decode(chunks: &[Value]) -> Result<Vec<Output>, Fault> {
    let mut reader = GeminiDecoder::default();
    let mut out = Vec::new();
    for c in chunks {
        reader.event(Bytes::from(format!("data: {c}\n\n")), &mut out)?;
    }
    reader.end(Ok(()), &mut out)?;
    Ok(out)
}

fn decode_body(v: &Value) -> Vec<Output> {
    let mut out = Vec::new();
    GeminiDecoder::default()
        .body(v, &mut out)
        .expect("a whole response");
    out
}

fn stream_of(replies: &[Output]) -> Result<String, Fault> {
    let mut enc = EventStream::default();
    let mut out = BytesMut::new();
    for reply in replies {
        enc.encode_into(reply, &mut out)?;
    }
    enc.end_into(&Ok(()), &mut out)?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn stream_events(stream: &str) -> Vec<Value> {
    let data = stream
        .split("\n\n")
        .filter_map(|e| e.lines().find_map(|l| l.strip_prefix("data: ")));
    data.filter_map(|d| serde_json::from_str(d).ok()).collect()
}

fn body_of(replies: &[Output]) -> Result<Value, Fault> {
    let mut whole = Assembled::default();
    replies.iter().try_for_each(|r| whole.push(r))?;
    GeminiGenerate.encode_whole(&whole)
}

fn uri(path: &str) -> http::Uri {
    path.parse().expect("a uri")
}

fn written(body: Value, edit: impl FnOnce(&mut Input)) -> Result<Value, Fault> {
    let mut chat = GeminiGenerate.decode_json(&body);
    edit(&mut chat);
    Ok(GeminiGenerate.encode_request(&chat)?.into())
}

#[test]
fn a_path_names_the_model_and_the_method() {
    let call = |p: &str| call_of(&uri(p)).map(|(m, s)| (m, s.to_owned()));
    assert_eq!(
        call("/v1beta/models/gemini-2.5-flash:generateContent"),
        Some(("gemini-2.5-flash".into(), "generateContent".into()))
    );
    assert_eq!(
        call("/gw/v1beta/models/g:streamGenerateContent?alt=sse"),
        Some(("g".into(), "streamGenerateContent".into()))
    );
    // An escaped model name is unescaped, as a gateway's route names one.
    assert_eq!(
        call("/v1beta/models/openai%2Fgpt-4o:generateContent"),
        Some(("openai/gpt-4o".into(), "generateContent".into()))
    );
    assert!(GeminiGenerate::streams(&uri(
        "/v1beta/models/g:streamGenerateContent?alt=sse"
    )));
    assert!(!GeminiGenerate::streams(&uri(
        "/v1beta/models/g:generateContent"
    )));
    for no in [
        "/v1beta/models/g:countTokens",
        "/v1beta/models:generateContent",
        "/v1/chat/completions",
        "/v1beta/tunedModels/g:generateContent",
    ] {
        assert_eq!(call(no), None, "{no}");
    }
}

#[test]
fn a_call_is_posted_to_the_path_of_its_model_and_method() {
    let mut chat = Input::default();
    chat.model = Some("models/gemini-2.5-flash".into());
    let target = |chat: &Input, stream| GeminiGenerate.target(chat, stream);
    assert_eq!(
        target(&chat, false).expect("a path"),
        Some("/models/gemini-2.5-flash:generateContent".into())
    );
    assert_eq!(
        target(&chat, true).expect("a path"),
        Some("/models/gemini-2.5-flash:streamGenerateContent?alt=sse".into())
    );
    chat.model = Some("../x".into());
    assert!(target(&chat, false).is_err());
    chat.model = None;
    assert!(target(&chat, false).is_err());
}

#[test]
fn a_request_reads_as_the_conversation_it_says_and_writes_back() {
    let body = json!({
        "systemInstruction": { "parts": [{ "text": "be brief" }] },
        "contents": [
            { "role": "user", "parts": [{ "text": "weather in Paris?" }] },
            { "role": "model", "parts": [
                { "text": "checking" },
                { "functionCall": { "name": "weather", "args": { "city": "Paris" } },
                  "thoughtSignature": "sig" },
            ] },
            { "role": "user", "parts": [
                { "functionResponse": { "name": "weather", "response": { "temp": 21 } } },
            ] },
        ],
        "tools": [{ "functionDeclarations": [{
            "name": "weather", "description": "d",
            "parameters": { "type": "OBJECT", "properties": { "city": { "type": "STRING" } } },
        }] }],
        "toolConfig": { "functionCallingConfig": { "mode": "ANY", "allowedFunctionNames": ["weather"] } },
        "generationConfig": { "temperature": 0.5, "topP": 0.9, "maxOutputTokens": 50,
            "stopSequences": ["END"], "candidateCount": 1 },
    });
    let chat = GeminiGenerate.decode_json(&body);
    assert!(chat.native.is_empty(), "{:?}", chat.native);
    assert_eq!(chat.messages[0].role, Role::System);
    assert_eq!(chat.messages[0].text(), "be brief");
    assert_eq!(chat.messages[1].role, Role::User);
    assert_eq!(chat.messages[2].role, Role::Assistant);
    let Content::ToolCall {
        id,
        name,
        arguments,
        native,
        ..
    } = &chat.messages[2].content[1]
    else {
        panic!("a call: {:?}", chat.messages[2]);
    };
    assert!(is_synthetic(id));
    assert_eq!(name, "weather");
    assert_eq!(arguments, r#"{"city":"Paris"}"#);
    assert_eq!(native.fields().get("thoughtSignature"), Some(&json!("sig")));
    // The result finds its call by name.
    let Content::ToolResult {
        id: answered,
        content,
        ..
    } = &chat.messages[3].content[0]
    else {
        panic!("a result");
    };
    assert_eq!(answered, id);
    assert_eq!(content, &[Content::from(r#"{"temp":21}"#)]);
    assert_eq!(chat.tools[0].name, "weather");
    assert_eq!(
        chat.tools[0].parameters,
        json!({ "type": "object", "properties": { "city": { "type": "string" } } })
    );
    assert_eq!(
        chat.options.tool_choice,
        Some(ToolChoice::Tool("weather".into()))
    );
    assert_eq!(chat.options.max_output_tokens, Some(50));
    assert_eq!(chat.options.temperature, Some(0.5));
    assert_eq!(chat.options.top_p, Some(0.9));
    assert_eq!(chat.options.stop, ["END"]);

    // Written again, the tools say JSON Schema, and no id the format made.
    let written = written(body, |_| {}).expect("a request");
    assert_eq!(
        written["contents"][1]["parts"][1]["functionCall"],
        json!({
        "name": "weather", "args": { "city": "Paris" } })
    );
    assert_eq!(
        written["contents"][1]["parts"][1]["thoughtSignature"],
        "sig"
    );
    assert_eq!(
        written["contents"][2]["parts"][0]["functionResponse"],
        json!({
        "name": "weather", "response": { "temp": 21 } })
    );
    assert_eq!(
        written["systemInstruction"],
        json!({ "parts": [{ "text": "be brief" }] })
    );
    assert_eq!(
        written["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"],
        json!({
        "type": "object", "properties": { "city": { "type": "string" } } })
    );
    assert_eq!(
        written["toolConfig"]["functionCallingConfig"],
        json!({
        "mode": "ANY", "allowedFunctionNames": ["weather"] })
    );
    assert_eq!(
        written["generationConfig"],
        json!({
        "temperature": 0.5, "topP": 0.9, "maxOutputTokens": 50, "stopSequences": ["END"] })
    );
    assert!(written.get("model").is_none());
}

#[test]
fn what_the_kind_has_no_form_for_stays_native_and_goes_back() {
    let body = json!({
        "contents": [{ "role": "user", "parts": [
            { "text": "hi" },
            { "videoMetadata": { "startOffset": "1s" }, "fileData": { "mimeType": "video/mp4", "fileUri": "gs://v" } },
        ] }],
        "safetySettings": [{ "category": "HARM_CATEGORY_HATE_SPEECH", "threshold": "BLOCK_NONE" }],
        "tools": [{ "googleSearch": {} }],
        "toolConfig": { "functionCallingConfig": { "mode": "VALIDATED" } },
        "generationConfig": { "seed": 3, "thinkingConfig": { "thinkingBudget": -1 }, "temperature": 1 },
    });
    let chat = GeminiGenerate.decode_json(&body);
    let native = chat.native.fields();
    for key in ["safetySettings", "tools", "toolConfig", "generationConfig"] {
        assert!(native.contains_key(key), "{key}");
    }
    assert_eq!(
        native["generationConfig"],
        json!({ "seed": 3, "thinkingConfig": { "thinkingBudget": -1 } })
    );
    assert_eq!(chat.options.temperature, Some(1.0));
    assert!(matches!(chat.messages[0].content[1], Content::Native(..)));
    let back = written(body.clone(), |_| {}).expect("a request");
    assert_eq!(back["safetySettings"], body["safetySettings"]);
    assert_eq!(back["tools"], body["tools"]);
    assert_eq!(back["toolConfig"], body["toolConfig"]);
    assert_eq!(back["contents"], body["contents"]);
    assert_eq!(back["generationConfig"]["seed"], 3);
    assert_eq!(back["generationConfig"]["temperature"], 1.0);
}

#[test]
fn media_reads_by_its_mime_type_and_writes_as_it_came() {
    let body = json!({ "contents": [{ "role": "user", "parts": [
        { "inlineData": { "mimeType": "image/png", "data": "aGk=" } },
        { "inlineData": { "mimeType": "audio/wav", "data": "aGk=" } },
        { "fileData": { "mimeType": "application/pdf", "fileUri": "https://x/y.pdf" } },
    ] }] });
    let chat = GeminiGenerate.decode_json(&body);
    let parts = &chat.messages[0].content;
    assert!(matches!(parts[0], Content::Image(..)));
    assert!(matches!(parts[1], Content::Audio(..)));
    assert!(matches!(parts[2], Content::Document(..)));
    let back = written(body.clone(), |_| {}).expect("a request");
    assert_eq!(back["contents"], body["contents"]);
}

#[test]
fn results_of_one_turn_are_one_content_and_name_their_tools() {
    let mut chat = Input::new([
        respan_gateway_conversation::Message::new(Role::User, "go"),
        {
            let mut m = respan_gateway_conversation::Message::new(Role::Assistant, "");
            m.content = vec![
                Content::tool_call("call_a", "one", "{}"),
                Content::tool_call("call_b", "two", r#"{"x":1}"#),
            ];
            m
        },
    ]);
    for (id, text) in [("call_b", "plain"), ("call_a", r#"{"k":1}"#)] {
        let mut m = respan_gateway_conversation::Message::new(Role::User, "");
        m.content = vec![Content::tool_result(id, vec![text.into()], id == "call_a")];
        chat.messages.push(m);
    }
    let v = GeminiGenerate.encode_request(&chat).expect("a request");
    let contents = &v["contents"];
    assert_eq!(contents.as_array().map(Vec::len), Some(3));
    assert_eq!(contents[1]["parts"][0]["functionCall"]["id"], "call_a");
    assert_eq!(
        contents[2]["parts"],
        json!([
            { "functionResponse": { "name": "two", "response": { "output": "plain" }, "id": "call_b" } },
            { "functionResponse": { "name": "one", "response": { "k": 1 }, "id": "call_a" } },
        ])
    );
    // A result of an unknown call is refused.
    chat.messages.push({
        let mut m = respan_gateway_conversation::Message::new(Role::User, "");
        m.content = vec![Content::tool_result("nope", vec!["x".into()], false)];
        m
    });
    assert!(GeminiGenerate.encode_request(&chat).is_err());
}

#[test]
fn structured_output_is_a_mime_type_and_a_schema() {
    let schema = json!({ "type": "object", "properties": { "a": { "type": "integer" } } });
    let body = json!({ "contents": [{ "parts": [{ "text": "x" }] }],
        "generationConfig": { "responseMimeType": "application/json",
            "responseSchema": { "type": "OBJECT", "properties": { "a": { "type": "INTEGER" } } } } });
    let chat = GeminiGenerate.decode_json(&body);
    assert!(chat.native.is_empty());
    assert_eq!(
        chat.options.response_format,
        Some(respan_gateway_conversation::ResponseFormat::JsonSchema {
            name: "response".into(),
            description: None,
            schema: schema.clone(),
            strict: Some(true),
        })
    );
    let back = written(body, |_| {}).expect("a request");
    assert_eq!(
        back["generationConfig"],
        json!({
        "responseMimeType": "application/json", "responseJsonSchema": schema })
    );
    let json_object = json!({ "contents": [{ "parts": [{ "text": "x" }] }],
        "generationConfig": { "responseMimeType": "application/json" } });
    let chat = GeminiGenerate.decode_json(&json_object);
    assert_eq!(
        chat.options.response_format,
        Some(respan_gateway_conversation::ResponseFormat::JsonObject)
    );
}

#[test]
fn what_gemini_cannot_say_is_refused_not_dropped() {
    let one = json!({ "contents": [{ "parts": [{ "text": "x" }] }] });
    let refused = |edit: fn(&mut Input)| written(one.clone(), edit).is_err();
    assert!(refused(|c| c.user = Some("u".into())));
    assert!(refused(|c| c.options.parallel_tool_calls = Some(false)));
    assert!(refused(|c| c.messages.clear()));
    assert!(refused(|c| {
        let mut m = respan_gateway_conversation::Message::new(Role::System, "late");
        m.role = Role::System;
        c.messages.push(m);
    }));
    assert!(refused(
        |c| c.messages[0].role = Role::native("anthropic-messages", "x")
    ));
    assert!(!refused(|c| c.options.parallel_tool_calls = Some(true)));
    // Another format's native fields are refused, own ones kept.
    assert!(refused(|c| {
        c.native
            .fields_mut("openai-chat")
            .insert("seed".into(), 1.into());
    }));
}

fn text_chunk(text: &str) -> Value {
    json!({ "candidates": [{ "content": { "role": "model", "parts": [{ "text": text }] }, "index": 0 }],
        "modelVersion": "gemini-x", "responseId": "r1" })
}

fn last_chunk(text: &str, reason: &str) -> Value {
    let mut v = text_chunk(text);
    v["candidates"][0]["finishReason"] = reason.into();
    v["usageMetadata"] = json!({ "promptTokenCount": 4, "candidatesTokenCount": 2,
        "thoughtsTokenCount": 3, "cachedContentTokenCount": 1, "totalTokenCount": 9 });
    v
}

#[test]
fn a_stream_is_a_block_of_text_a_finish_and_usage() {
    let events =
        decode(&[text_chunk("Hel"), text_chunk("lo"), last_chunk("!", "STOP")]).expect("events");
    let mut usage = Usage::default();
    for e in &events {
        if let Output::Usage(u) = e {
            usage += u;
        }
    }
    assert_eq!(
        usage,
        Usage {
            input_tokens: 4,
            output_tokens: 5,
            cached_input_tokens: 1,
            cache_write_input_tokens: 0,
            reasoning_tokens: 3
        }
    );
    let words: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Output::Delta {
                delta: Delta::Text(t),
                block,
            } => Some((*block, t.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(words, [(0, "Hel"), (0, "lo"), (0, "!")]);
    assert!(matches!(&events[0], Output::Start(s) if s.id == "r1" && s.model == "gemini-x"));
    assert!(events.contains(&Output::finished(Finish::Stop)));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Output::BlockStart { .. }))
            .count(),
        1
    );
}

#[test]
fn a_stream_that_ends_without_a_finish_was_cut() {
    let cut = decode(&[text_chunk("Hel")]);
    assert!(cut.is_err());
}

#[test]
fn a_function_call_is_a_tool_block_and_stop_with_it_is_a_tool_use() {
    let chunk = json!({ "candidates": [{ "content": { "role": "model", "parts": [
        { "text": "ok" },
        { "functionCall": { "name": "f", "args": { "a": 1 } }, "thoughtSignature": "s" },
    ] }, "finishReason": "STOP" }], "responseId": "r" });
    let events = decode(std::slice::from_ref(&chunk)).expect("events");
    let call = events.iter().find_map(|e| match e {
        Output::BlockStart {
            kind: Block::ToolCall { id, name },
            ..
        } => Some((id.clone(), name.clone())),
        _ => None,
    });
    assert_eq!(call, Some(("gemini_call_r_1".into(), "f".into())));
    assert!(events.contains(&Output::finished(Finish::ToolUse)));
    assert!(events.contains(&Output::arguments(1, r#"{"a":1}"#)));
    // The signature is kept for this format; another drops it.
    let fields = events.iter().find_map(|e| match e {
        Output::Native(n) if n.value["type"] == "block_fields" => Some(Value::clone(&n.value)),
        _ => None,
    });
    assert_eq!(
        fields,
        Some(json!({ "type": "block_fields", "block": 1, "fields": { "thoughtSignature": "s" } }))
    );

    assert_eq!(decode_body(&chunk), events);
}

#[test]
fn finish_reasons_map_to_the_kinds_and_back() {
    let reason = |r: &str| {
        decode_body(&last_chunk("x", r))
            .into_iter()
            .find_map(|e| match e {
                Output::Finish(f) => Some(f),
                _ => None,
            })
    };
    assert_eq!(reason("STOP"), Some(Finish::Stop));
    assert_eq!(reason("MAX_TOKENS"), Some(Finish::Length));
    assert_eq!(reason("SAFETY"), Some(Finish::Filtered));
    let recitation = reason("RECITATION").expect("a finish");
    assert_eq!(reason_of(&recitation).ok(), Some("RECITATION"));
    assert_eq!(FINISH.reason(&Finish::Filtered).ok(), Some("SAFETY"));
    assert_eq!(FINISH.reason(&Finish::ToolUse).ok(), Some("STOP"));
    let malformed = reason("MALFORMED_FUNCTION_CALL").expect("a finish");
    assert_eq!(reason_of(&malformed).ok(), Some("MALFORMED_FUNCTION_CALL"));
}

#[test]
fn a_blocked_prompt_is_filtered_and_written_as_it_came() {
    let v = json!({ "promptFeedback": { "blockReason": "SAFETY" },
        "usageMetadata": { "promptTokenCount": 3, "totalTokenCount": 3 } });
    let events = decode_body(&v);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Output::Finish(Finish::Native { .. })))
    );
    let again = body_of(&events).expect("a body");
    assert_eq!(again["promptFeedback"], v["promptFeedback"]);
    assert!(again.get("candidates").is_none());
    assert_eq!(again["usageMetadata"]["promptTokenCount"], 3);
}

#[test]
fn a_body_is_written_from_the_events_it_decoded_to() {
    let v = json!({
        "candidates": [{
            "content": { "role": "model", "parts": [
                { "text": "think", "thought": true },
                { "text": "answer" },
                { "functionCall": { "name": "f", "args": { "a": 1 }, "id": "c1" } },
            ] },
            "finishReason": "STOP", "index": 0,
            "safetyRatings": [{ "category": "X", "probability": "NEGLIGIBLE" }],
        }],
        "usageMetadata": { "promptTokenCount": 4, "candidatesTokenCount": 2, "thoughtsTokenCount": 3,
            "totalTokenCount": 9, "promptTokensDetails": [{ "modality": "TEXT", "tokenCount": 4 }] },
        "modelVersion": "gemini-x", "responseId": "r1",
    });
    let again = body_of(&decode_body(&v)).expect("a body");
    assert_eq!(again, v);
}

#[test]
fn a_stream_is_written_a_response_per_delta_and_a_last_with_the_finish() {
    let replies = [
        Output::start("r1", "gemini-x"),
        Output::block_start(0, Block::Text),
        Output::text(0, "Hel"),
        Output::text(0, "lo"),
        Output::block_end(0),
        Output::block_start(
            1,
            Block::ToolCall {
                id: "c".into(),
                name: "f".into(),
            },
        ),
        Output::arguments(1, r#"{"a":"#),
        Output::arguments(1, "1}"),
        Output::block_end(1),
        Output::Usage(Usage {
            input_tokens: 4,
            output_tokens: 5,
            cached_input_tokens: 1,
            cache_write_input_tokens: 0,
            reasoning_tokens: 3,
        }),
        Output::finished(Finish::ToolUse),
    ];
    let events = stream_events(&stream_of(&replies).expect("a stream"));
    let parts: Vec<Value> = events
        .iter()
        .flat_map(|e| {
            e["candidates"][0]["content"]["parts"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .collect();
    assert_eq!(
        parts,
        [
            json!({ "text": "Hel" }),
            json!({ "text": "lo" }),
            json!({ "functionCall": { "name": "f", "args": { "a": 1 }, "id": "c" } }),
        ]
    );
    let last = events.last().expect("a last response");
    assert_eq!(last["candidates"][0]["finishReason"], "STOP");
    assert_eq!(
        last["usageMetadata"],
        json!({
        "promptTokenCount": 4, "candidatesTokenCount": 2, "thoughtsTokenCount": 3,
        "cachedContentTokenCount": 1, "totalTokenCount": 9 })
    );
    assert_eq!(last["responseId"], "r1");
    assert_eq!(events[0]["modelVersion"], "gemini-x");
    assert_eq!(
        events
            .iter()
            .filter(|e| e.get("usageMetadata").is_some())
            .count(),
        1
    );
}

#[test]
fn what_another_format_says_is_dropped_and_what_none_can_is_refused() {
    let alien = Output::native("anthropic-messages", json!({ "type": "x" }));
    let stream = stream_of(std::slice::from_ref(&alien)).expect("a stream");
    assert!(!stream.contains("\"x\""), "{stream}");
    let body = body_of(&[alien]).expect("a body");
    assert!(!body.to_string().contains("\"x\""), "{body}");
    let video = Output::native(
        GEMINI,
        json!({ "type": "part", "part": { "executableCode": {} } }),
    );
    assert!(stream_of(std::slice::from_ref(&video)).is_ok());
    // A cache write cannot be said.
    let mut usage = Usage::new(5, 1);
    usage.cache_write_input_tokens = 2;
    assert!(stream_of(&[Output::Usage(usage)]).is_err());
}

#[test]
fn an_error_body_is_googles() {
    let fault = Fault::bad_request("no good");
    let body = GeminiGenerate::error_body(&fault);
    assert_eq!(
        body,
        json!({ "error": { "code": 400, "message": "no good", "status": "INVALID_ARGUMENT" } })
    );
}

/// A history in which the model called `f` and `g` at once, `g` signed,
/// written for `model`.
fn signed_for(model: &str) -> Value {
    let body = json!({ "contents": [
        { "role": "user", "parts": [{ "text": "go" }] },
        { "role": "model", "parts": [
            { "functionCall": { "name": "f", "args": {} } },
            { "functionCall": { "name": "g", "args": {} }, "thoughtSignature": "own" }] },
        { "role": "user", "parts": [
            { "functionResponse": { "name": "f", "response": { "a": 1 } } },
            { "functionResponse": { "name": "g", "response": { "b": 2 } } }] },
    ] });
    written(body, |chat| chat.model = Some(model.into())).expect("a request")
}

#[test]
fn a_gemini_3_turns_first_call_without_a_signature_takes_the_placeholder() {
    let parts = &signed_for("gemini-3-flash-preview")["contents"][1]["parts"];
    assert_eq!(parts[0]["thoughtSignature"], SIGNATURE_PLACEHOLDER);
    assert_eq!(parts[1]["thoughtSignature"], "own");

    let parts = &signed_for("models/gemini-2.5-flash")["contents"][1]["parts"];
    assert!(parts[0].get("thoughtSignature").is_none(), "{parts}");
}

/// Writes each value as one event whose data spans two lines.
struct TwoLines;

impl respan_gateway_http::Encoder<Output> for TwoLines {
    fn encode(&mut self, _: &Output, out: &mut Vec<Bytes>) -> Result<(), Fault> {
        out.push(respan_gateway_sse::event(None, "{\"a\":\n1}").into());
        Ok(())
    }
}

#[test]
fn a_json_array_element_joins_the_data_lines_of_its_event() {
    let mut array = JsonArray::new(Box::new(TwoLines));
    let mut out = BytesMut::new();
    for _ in 0..2 {
        array
            .encode_into(&Output::Finish(Finish::Stop), &mut out)
            .expect("written");
    }
    array.end_into(&Ok(()), &mut out).expect("ended");
    let elements: Value = serde_json::from_slice(&out).expect("one JSON array");
    assert_eq!(elements, json!([{ "a": 1 }, { "a": 1 }]));
}
