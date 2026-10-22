use respan_gateway_core::FaultKind;
use respan_gateway_core::NativeFields;
use respan_gateway_http::bytes::Bytes;
use respan_gateway_realtime::{
    AudioFormat, Cause, Input, Output, Purpose, Session, Status, ToolResult,
};
use serde_json::{Value, json};

use super::{input, output};

/// The events of `frames`, a client's, after the session its handshake opens.
fn inputs(frames: &[Value]) -> Vec<Input> {
    let mut client = input::Client::new(Some("gpt-realtime".into()), None);
    let mut events = Vec::new();
    for frame in frames {
        client
            .read(Bytes::from(frame.to_string()), &mut events)
            .expect("read");
    }
    events
}

/// What `events` are written as, to a provider.
fn sent(events: &[Input]) -> Result<Vec<Value>, respan_gateway_core::Fault> {
    let mut provider = input::Provider::new("gpt-realtime".into(), false);
    let mut frames = Vec::new();
    for event in events {
        provider.write(event, &mut frames)?;
    }
    Ok(frames
        .iter()
        .map(|f| serde_json::from_slice(f).expect("JSON"))
        .collect())
}

fn outputs(frames: &[Value]) -> Vec<Output> {
    let mut provider = output::Provider::default();
    let mut events = Vec::new();
    for frame in frames {
        provider
            .read(Bytes::from(frame.to_string()), &mut events)
            .expect("read");
    }
    events
}

fn heard(events: &[Output]) -> Result<Vec<Value>, respan_gateway_core::Fault> {
    let mut client = output::Client::default();
    let mut frames = Vec::new();
    for event in events {
        client.write(event, &mut frames)?;
    }
    Ok(frames
        .iter()
        .map(|f| serde_json::from_slice(f).expect("JSON"))
        .collect())
}

#[test]
fn every_client_event_is_written_back_as_it_came() {
    let frames = [
        json!({"type": "session.update", "event_id": "e1", "session": {"type": "realtime",
            "instructions": "Be brief.", "output_modalities": ["audio"],
            "audio": {"input": {"format": {"type": "audio/pcm", "rate": 24000},
                "transcription": {"model": "gpt-4o-mini-transcribe", "language": "en"},
                "noise_reduction": {"type": "far_field"},
                "turn_detection": {"type": "semantic_vad", "eagerness": "low",
                    "create_response": true, "interrupt_response": false}},
                "output": {"format": {"type": "audio/pcmu"}, "voice": "marin", "speed": 1.1}},
            "tools": [{"type": "function", "name": "f", "parameters": {"type": "object"}}],
            "tool_choice": {"type": "function", "name": "f"}, "max_output_tokens": "inf"}}),
        json!({"type": "input_audio_buffer.append", "event_id": "e2", "audio": "AAEC"}),
        json!({"type": "input_audio_buffer.commit"}),
        json!({"type": "conversation.item.create", "previous_item_id": "root",
            "item": {"id": "i1", "type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "hi"},
                    {"type": "input_image", "image_url": "data:image/png;base64,AA=="}]}}),
        json!({"type": "conversation.item.create", "item": {"type": "message",
            "role": "assistant", "content": [{"type": "output_text", "text": "hello"}]}}),
        json!({"type": "conversation.item.create", "item": {"type": "function_call_output",
            "call_id": "c1", "output": "{\"ok\":true}"}}),
        json!({"type": "response.create", "response": {"instructions": "short",
            "max_output_tokens": 200}}),
        json!({"type": "response.cancel", "event_id": "e3"}),
        json!({"type": "conversation.item.truncate", "item_id": "i2", "content_index": 0,
            "audio_end_ms": 10}),
        json!({"type": "input_audio_buffer.append", "audio": "not base64!"}),
        json!({"type": "session.update", "session": {"type": "realtime", "modalities": ["text"]}}),
        json!("not an event"),
    ];
    let events = inputs(&frames);
    let kinds: Vec<&str> = events.iter().map(kind).collect();
    assert_eq!(
        kinds,
        [
            "session",
            "audio",
            "commit",
            "message",
            "message",
            "tool_result",
            "respond",
            "cancel",
            "native",
            "native",
            "session",
            "native"
        ]
    );
    assert_eq!(sent(&events).expect("written"), frames);
}

fn kind(event: &Input) -> &'static str {
    match event {
        Input::Session(_) => "session",
        Input::Audio { .. } => "audio",
        Input::Commit { .. } => "commit",
        Input::Message { .. } => "message",
        Input::ToolResult(_) => "tool_result",
        Input::Respond { .. } => "respond",
        Input::Cancel { .. } => "cancel",
        _ => "native",
    }
}

#[test]
fn a_client_event_is_read_as_the_kinds() {
    let frames = [
        json!({"type": "session.update", "session": {"type": "realtime",
            "audio": {"input": {"format": {"type": "audio/pcma"}}}}}),
        json!({"type": "input_audio_buffer.append", "audio": "AAE"}),
    ];
    let events = inputs(&frames);
    // The first update is the session the handshake opens.
    let Input::Session(session) = &events[0] else {
        panic!("{events:?}")
    };
    assert_eq!(session.model.as_deref(), Some("gpt-realtime"));
    assert_eq!(session.input_audio, Some(AudioFormat::ALAW));
    let Input::Audio { audio, native } = &events[1] else {
        panic!("{events:?}")
    };
    assert_eq!(
        (audio.format, &audio.data[..]),
        (AudioFormat::ALAW, &[0, 1][..])
    );
    assert_eq!(*native, None);
    // Written padded: the same audio.
    assert_eq!(sent(&events).expect("written")[1]["audio"], "AAE=");
    // A first frame of another event follows a session of the model alone.
    let events = inputs(&frames[1..]);
    assert!(matches!(&events[0], Input::Session(s) if s.model.as_deref() == Some("gpt-realtime")));
    assert!(matches!(events[1], Input::Audio { .. }));
}

#[test]
fn a_session_of_nothing_but_its_model_is_no_frame() {
    let mut opening = Session::default();
    opening.model = Some("gpt-realtime".into());
    let session = Input::Session(opening);
    let respond = Input::Respond { native: None };
    // A session a plugin opens with says something: it is written, typed.
    let mut capped = Session::default();
    capped.max_output_tokens = Some(100);
    let written = sent(&[Input::Session(capped), session, respond]).expect("written");
    assert_eq!(
        written,
        [
            json!({"type": "session.update", "session": {"type": "realtime",
                "max_output_tokens": 100}}),
            json!({"type": "response.create"})
        ]
    );
    let result = Input::ToolResult(ToolResult::new("c1", "{}"));
    assert_eq!(
        sent(&[result]).expect("written"),
        [json!({"type": "conversation.item.create",
            "item": {"type": "function_call_output", "call_id": "c1", "output": "{}"}})]
    );
}

#[test]
fn a_tools_schema_names_its_types_as_openai_takes_them() {
    let gemini = json!({"type": "OBJECT", "properties": {
        "city": {"type": "STRING"},
        "type": {"type": ["INTEGER", "NULL"]},
        "days": {"type": "ARRAY", "items": {"type": "OBJECT",
            "properties": {"at": {"anyOf": [{"type": "NUMBER"}, {"type": "BOOLEAN"}]}}}}}});
    let mut session = Session::default();
    session.tools = Some(vec![respan_gateway_realtime::Tool::new("weather", gemini)]);
    let written = sent(&[Input::Session(session)]).expect("written");
    assert_eq!(
        written[0]["session"]["tools"][0]["parameters"],
        json!({"type": "object", "properties": {
            "city": {"type": "string"},
            "type": {"type": ["integer", "null"]},
            "days": {"type": "array", "items": {"type": "object",
                "properties": {"at": {"anyOf": [{"type": "number"}, {"type": "boolean"}]}}}}}})
    );
}

#[test]
fn another_formats_input_is_dropped_where_it_cannot_change_the_answer() {
    let gemini = |v: Value| {
        Some(NativeFields::new(
            "gemini-live",
            v.as_object().cloned().expect("object"),
        ))
    };
    let shaped = Input::Respond {
        native: gemini(json!({"clientContent": {}})),
    };
    assert_eq!(
        sent(&[shaped]).expect("written"),
        [json!({"type": "response.create"})]
    );
    let start = Input::native(
        "gemini-live",
        json!({"realtimeInput": {"activityStart": {}}}),
    );
    assert_eq!(sent(&[start]).expect("written"), Vec::<Value>::new());
    let mut sampled = Session::default();
    sampled.native = gemini(json!({"setup": {"generationConfig": {"temperature": 0.2}}}));
    for (event, field) in [
        (
            Input::Session(sampled),
            "`setup.generationConfig.temperature`",
        ),
        (
            Input::native("gemini-live", json!({"realtimeInput": {"video": {}}})),
            "`realtimeInput.video`",
        ),
    ] {
        let refused = sent(&[event]).expect_err("refused");
        assert!(refused.is(FaultKind::Client), "{refused}");
        assert_eq!(
            refused.message(),
            format!("{field} cannot be translated to openai-realtime")
        );
    }
}

#[test]
fn audio_in_another_format_is_converted_to_the_sessions() {
    let audio = |format, data: Vec<u8>| Input::Audio {
        audio: respan_gateway_realtime::Audio {
            data: Bytes::from(data),
            format,
        },
        native: None,
    };
    let second: Vec<u8> = (0..16_000i16)
        .flat_map(|i| (i % 100).to_le_bytes())
        .collect();
    let written = sent(&[audio(AudioFormat::pcm16(16_000), second)]).expect("written");
    let data = crate::json::unbase64(&written[0]["audio"]).expect("base64");
    // A second at 24 kHz, but what the resampler still holds.
    assert!((47_000..=48_000).contains(&data.len()), "{}", data.len());
    let refused = sent(&[audio(AudioFormat::pcm16(500), vec![0; 4])]).expect_err("refused");
    assert!(refused.is(FaultKind::Client), "{refused}");
}

#[test]
fn every_server_event_is_written_back_as_it_came() {
    let usage = json!({"total_tokens": 7, "input_tokens": 5, "output_tokens": 2,
        "input_token_details": {"cached_tokens": 0, "text_tokens": 1, "audio_tokens": 4},
        "output_token_details": {"text_tokens": 2, "audio_tokens": 0}});
    let frames = [
        json!({"type": "session.created", "event_id": "s1", "session": {"type": "realtime",
            "object": "realtime.session", "id": "sess_1", "model": "gpt-realtime",
            "output_modalities": ["audio"], "max_output_tokens": "inf", "tools": [],
            "tool_choice": "auto", "instructions": "",
            "audio": {"input": {"format": {"type": "audio/pcm", "rate": 24000},
                "transcription": null, "turn_detection": {"type": "server_vad",
                    "threshold": 0.5, "silence_duration_ms": 200, "idle_timeout_ms": null}},
                "output": {"format": {"type": "audio/pcmu", "rate": 24000}, "voice": "alloy"}}}}),
        json!({"type": "input_audio_buffer.speech_started", "audio_start_ms": 0,
            "item_id": "i1", "event_id": "s2"}),
        json!({"type": "input_audio_buffer.speech_stopped", "audio_end_ms": 900,
            "item_id": "i1"}),
        json!({"type": "conversation.item.input_audio_transcription.delta", "item_id": "i1",
            "content_index": 0, "delta": "hel"}),
        json!({"type": "conversation.item.input_audio_transcription.completed",
            "item_id": "i1", "content_index": 0, "transcript": "hello",
            "usage": {"type": "duration", "seconds": 1}}),
        json!({"type": "response.created", "response": {"id": "r1", "status": "in_progress",
            "output": [], "usage": null}}),
        json!({"type": "response.output_item.added", "response_id": "r1", "output_index": 1,
            "item": {"id": "i3", "type": "function_call", "call_id": "c1", "name": "f"}}),
        json!({"type": "response.output_audio.delta", "response_id": "r1", "item_id": "i2",
            "output_index": 0, "content_index": 0, "delta": "AAEC"}),
        json!({"type": "response.output_text.delta", "item_id": "i2", "delta": "hi"}),
        json!({"type": "response.output_audio_transcript.delta", "item_id": "i2", "delta": "hi"}),
        json!({"type": "response.function_call_arguments.delta", "call_id": "c1", "delta": "{"}),
        json!({"type": "response.function_call_arguments.done", "call_id": "c1",
            "item_id": "i3", "name": "f", "arguments": "{}"}),
        json!({"type": "response.done", "response": {"id": "r1", "status": "completed",
            "status_details": null, "usage": usage}}),
        json!({"type": "response.done", "event_id": "s4", "response": {"status": "cancelled",
            "status_details": {"type": "cancelled", "reason": "turn_detected"}}}),
        json!({"type": "response.done", "response": {"status": "failed",
            "status_details": {"type": "failed", "error": {"code": "x"}}}}),
        json!({"type": "response.done", "response": {"status": "incomplete",
            "status_details": {"type": "incomplete", "reason": "something_new"}}}),
        json!({"type": "error", "event_id": "s3", "error": {"type": "invalid_request_error",
            "code": "invalid_value", "message": "no", "param": null, "event_id": "e9"}}),
        json!({"type": "rate_limits.updated", "rate_limits": []}),
    ];
    let events = outputs(&frames);
    let statuses: Vec<&Status> = events
        .iter()
        .filter_map(|e| match e {
            Output::ResponseDone { status, .. } => Some(status),
            _ => None,
        })
        .collect();
    assert_eq!(
        statuses[..3],
        [&Status::Completed, &Status::Interrupted, &Status::Failed]
    );
    assert!(matches!(statuses[3], Status::Native { reason, .. } if reason == "incomplete"));
    let Output::Usage(used) = &events[12] else {
        panic!("{events:?}")
    };
    assert_eq!((used.tokens.input_tokens, used.input_audio_tokens), (5, 4));
    let Output::Audio { audio, .. } = &events[7] else {
        panic!("{events:?}")
    };
    assert_eq!(audio.format, AudioFormat::MULAW);
    assert_eq!(heard(&events).expect("written"), frames);
}

#[test]
fn a_call_that_does_not_name_its_tool_is_named_by_its_item() {
    let frames = [
        json!({"type": "response.output_item.added",
            "item": {"type": "function_call", "call_id": "c1", "name": "weather"}}),
        json!({"type": "response.function_call_arguments.done", "call_id": "c1",
            "arguments": "{}"}),
    ];
    let events = outputs(&frames);
    assert!(matches!(&events[1], Output::ToolCall { name, .. } if name == "weather"));
}

#[test]
fn usage_a_filter_edits_is_written_in_the_response_done() {
    let frames = [
        json!({"type": "response.done", "response": {"status": "completed",
        "usage": {"input_tokens": 5, "output_tokens": 2, "total_tokens": 7}}}),
    ];
    let mut events = outputs(&frames);
    if let Output::Usage(usage) = &mut events[0] {
        usage.tokens.output_tokens = 3;
        usage.output_audio_tokens = 1;
    }
    let written = heard(&events).expect("written");
    assert_eq!(
        written[0]["response"]["usage"],
        json!({"input_tokens": 5, "output_tokens": 3, "total_tokens": 7,
            "output_token_details": {"audio_tokens": 1}})
    );
}

#[test]
fn a_response_of_another_format_is_told_of_as_openai_does() {
    let response = [
        Output::ResponseStart {
            id: None,
            native: None,
        },
        Output::Audio {
            audio: respan_gateway_realtime::Audio {
                data: Bytes::from_static(&[0; 4]),
                format: AudioFormat::pcm16(24_000),
            },
            native: None,
        },
        Output::Transcript {
            text: "Hi.".into(),
            native: None,
        },
        Output::ToolCall {
            id: "c1".into(),
            name: "f".into(),
            arguments: "{}".into(),
            native: None,
        },
        Output::Usage(Default::default()),
        Output::ResponseDone {
            status: Status::Completed,
            native: Some(NativeFields::new("gemini-live", Default::default())),
        },
    ];
    let written = heard(&response).expect("written");
    let kinds: Vec<&str> = written
        .iter()
        .map(|v| v["type"].as_str().expect("typed"))
        .collect();
    assert_eq!(
        kinds,
        [
            "response.created",
            "response.output_item.added",
            "conversation.item.added",
            "response.content_part.added",
            "response.output_audio.delta",
            "response.output_audio_transcript.delta",
            "response.output_audio.done",
            "response.output_audio_transcript.done",
            "response.content_part.done",
            "conversation.item.done",
            "response.output_item.done",
            "response.output_item.added",
            "conversation.item.added",
            "response.function_call_arguments.delta",
            "response.function_call_arguments.done",
            "conversation.item.done",
            "response.output_item.done",
            "response.done",
        ]
    );
    let done = &written[17]["response"];
    assert_eq!(done["id"], written[0]["response"]["id"]);
    assert_eq!(done["output"][0]["content"][0]["transcript"], "Hi.");
    assert_eq!(done["output"][1]["call_id"], "c1");
    assert!(written.iter().all(|v| v["event_id"].is_string()));
    let ids: std::collections::HashSet<&Value> = written.iter().map(|v| &v["event_id"]).collect();
    assert_eq!(ids.len(), written.len());
}

#[test]
fn an_error_is_written_as_openais() {
    let error = |cause| Output::Error {
        cause,
        code: Some("another format's code".into()),
        message: "no".into(),
        native: None,
    };
    let written = heard(&[error(Cause::Invalid), error(Cause::Provider)]).expect("written");
    assert_eq!(
        written[0]["error"],
        json!({"type": "invalid_request_error", "message": "no"})
    );
    assert_eq!(written[1]["error"]["type"], "server_error");
}

#[test]
fn an_error_says_its_cause_as_any_format_reads_it() {
    let error = |error: Value| json!({"type": "error", "error": error});
    let read = outputs(&[
        error(json!({"type": "invalid_request_error", "message": "bad"})),
        error(
            json!({"type": "invalid_request_error", "code": "response_cancel_not_active",
            "message": "none"}),
        ),
        error(json!({"type": "server_error", "message": "down"})),
    ]);
    let causes: Vec<Cause> = read
        .iter()
        .filter_map(|o| match o {
            Output::Error { cause, .. } => Some(*cause),
            _ => None,
        })
        .collect();
    assert_eq!(
        causes,
        [Cause::Invalid, Cause::NothingToCancel, Cause::Provider]
    );
    // OpenAI's own error is written back as it came.
    let written = heard(&read[1..2]).expect("written");
    assert_eq!(written[0]["error"]["code"], "response_cancel_not_active");
}

#[test]
fn each_whole_transcript_is_an_item_of_its_own() {
    let transcript = |text: &str, complete| Output::InputTranscript {
        text: text.into(),
        complete,
        native: None,
    };
    let frames = heard(&[
        transcript("one", false),
        transcript("one", true),
        transcript("two", false),
        transcript("two", true),
    ])
    .expect("heard");
    let items: Vec<_> = frames.iter().map(|f| f["item_id"].clone()).collect();
    assert_eq!(items[0], items[1]);
    assert_eq!(items[2], items[3]);
    assert_ne!(items[1], items[2]);
}

#[test]
fn a_sessions_type_is_its_purpose() {
    let events = inputs(&[json!({"type": "session.update",
        "session": {"type": "transcription"}})]);
    let purposes: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Input::Session(s) => Some(s.purpose),
            _ => None,
        })
        .collect();
    assert_eq!(purposes.last(), Some(&Some(Purpose::Transcription)));
    let mut transcription = Session::default();
    transcription.purpose = Some(Purpose::Transcription);
    let frames = heard(&[Output::Opened(transcription)]).expect("heard");
    assert_eq!(frames[0]["session"]["type"], "transcription");
    let frames = heard(&[Output::Opened(Session::default())]).expect("heard");
    assert_eq!(frames[0]["session"]["type"], "realtime");
}

#[test]
fn a_transcription_is_opened_by_the_model_routing_chose() {
    let mut client = input::Client::new(
        Some("gpt-4o-transcribe".into()),
        Some(Purpose::Transcription),
    );
    let mut events = Vec::new();
    let update = json!({"type": "session.update", "session": {
        "audio": {"input": {"transcription": {"model": "gpt-4o-mini-transcribe"}}}}});
    client
        .read(Bytes::from(update.to_string()), &mut events)
        .expect("read");
    let [Input::Session(session)] = events.as_slice() else {
        panic!("{events:?}");
    };
    assert_eq!(session.purpose, Some(Purpose::Transcription));

    let mut provider = input::Provider::new("gpt-4o-transcribe".into(), true);
    let mut frames = Vec::new();
    provider.write(&events[0], &mut frames).expect("written");
    let v: Value = serde_json::from_slice(&frames[0]).expect("JSON");
    assert_eq!(v["session"]["type"], "transcription");
    assert_eq!(v["session"].get("model"), None);
    assert_eq!(
        v["session"]["audio"]["input"]["transcription"]["model"],
        "gpt-4o-transcribe"
    );
    // A later session's transcription settings keep the model routing chose.
    let mut later = Session::default();
    later.input_transcription = Some(true);
    let mut frames = Vec::new();
    provider
        .write(&Input::Session(later), &mut frames)
        .expect("written");
    let v: Value = serde_json::from_slice(&frames[0]).expect("JSON");
    assert_eq!(
        v["session"]["audio"]["input"]["transcription"]["model"],
        "gpt-4o-transcribe"
    );
}

#[test]
fn a_handshake_asks_for_a_transcription_at_its_intent() {
    let purpose = |at: &str| super::purpose_at(&at.parse().expect("a URI"));
    let intent = "/v1/realtime?model=m&intent=transcription";
    assert_eq!(purpose(intent), Some(Purpose::Transcription));
    assert_eq!(purpose("/v1/realtime?model=m"), None);
}
