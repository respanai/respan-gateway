use respan_gateway_conversation::{Message, Role};
use respan_gateway_core::{Fault, FaultKind, NativeFields};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_realtime::{
    Audio, AudioFormat, Cause, Input, Modality, Output, Purpose, Session, Status, Tool, ToolResult,
    Turns, Usage, Vad,
};
use respan_gateway_ws::CloseCode;
use serde_json::{Value, json};

use super::{NAME, input, output};
use crate::session as spec;

/// The events of a client's `frames`, its handshake asking for `model`.
fn inputs(model: Option<&str>, frames: &[Value]) -> Vec<Input> {
    let mut client = input::Client::new(model.map(str::to_owned));
    let mut events = Vec::new();
    for frame in frames {
        client
            .read(Bytes::from(frame.to_string()), &mut events)
            .expect("read");
    }
    events
}

/// What `events` are written as, to a provider of no model routing chose.
fn sent(events: &[Input]) -> Result<Vec<Value>, Fault> {
    let mut provider = input::Provider::new(None, None, spec::say().0);
    let mut frames = Vec::new();
    for event in events {
        provider.write(event, &mut frames)?;
    }
    Ok(json_of(&frames))
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

fn heard(events: &[Output]) -> Result<Vec<Value>, Fault> {
    let mut client = output::Client::default();
    let mut frames = Vec::new();
    for event in events {
        client.write(event, &mut frames)?;
    }
    Ok(json_of(&frames))
}

fn json_of(frames: &[Bytes]) -> Vec<Value> {
    frames
        .iter()
        .map(|f| serde_json::from_slice(f).expect("JSON"))
        .collect()
}

/// A session of another format: no native fields.
fn session(model: &str) -> Session {
    let mut s = Session::default();
    s.model = Some(model.into());
    s
}

#[test]
fn every_client_event_is_written_back_as_it_came() {
    let frames = [
        json!({"type": "session.start", "event_id": "e1", "session": {"model": "gpt-live-1",
            "instructions": "Be brief.", "store": true,
            "audio": {"format": {"type": "audio/pcmu", "rate": 8000},
                "output": {"voice": "marin"}},
            "delegation": {"type": "client", "acknowledgement_delay_ms": 200},
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}]}}),
        json!({"type": "session.input_audio.append", "event_id": "e2", "audio": "AAEC"}),
        json!({"type": "session.update", "session": {"delegation": {"type": "client",
            "acknowledgement_delay_ms": 0}}}),
        json!({"type": "session.commentary.append", "content": "Checking.",
            "delegation_id": "d1"}),
        json!({"type": "session.input_audio.mute"}),
        json!({"type": "session.close"}),
    ];
    let events = inputs(None, &frames);
    assert_eq!(events.len(), frames.len(), "{events:?}");
    let Input::Session(started) = &events[0] else {
        panic!("a session: {:?}", events[0]);
    };
    assert_eq!(started.model.as_deref(), Some("gpt-live-1"));
    assert_eq!(started.instructions.as_deref(), Some("Be brief."));
    assert_eq!(started.voice.as_deref(), Some("marin"));
    assert_eq!(started.input_audio, Some(AudioFormat::MULAW));
    assert_eq!(started.output_audio, Some(AudioFormat::MULAW));
    assert_eq!(
        started.input_transcription,
        Some(true),
        "it always transcribes"
    );
    assert!(matches!(&events[1], Input::Audio { audio, .. }
        if audio.format == AudioFormat::MULAW && audio.data[..] == [0, 1, 2]));
    assert!(matches!(&events[2], Input::Session(s) if s.native.is_some()));
    assert!(matches!(&events[3], Input::Native(_)));
    assert_eq!(sent(&events).expect("written"), frames);
}

#[test]
fn a_session_names_the_model_routing_chose_else_the_handshakes_else_its_own() {
    let start = json!({"type": "session.start", "session": {"model": "alias"}});
    let events = inputs(Some("asked"), &[start]);
    let written = |chosen: Option<&str>, asked: Option<&str>| {
        let (chosen, asked) = (chosen.map(str::to_owned), asked.map(str::to_owned));
        let mut provider = input::Provider::new(chosen, asked, spec::say().0);
        let mut frames = Vec::new();
        provider
            .write(&events[0], &mut frames)
            .map(|_| json_of(&frames))
    };
    let model = |frames: Vec<Value>| frames[0]["session"]["model"].clone();
    assert_eq!(
        model(written(Some("gpt-live-1"), None).expect("written")),
        "gpt-live-1"
    );
    // Routing judged the handshake's model, so a start naming another
    // cannot go round it.
    assert_eq!(model(written(None, Some("x")).expect("written")), "x");
    assert_eq!(model(written(None, None).expect("written")), "alias");

    // A client whose first event is not its start opens with the
    // handshake's model.
    let audio = json!({"type": "session.input_audio.append", "audio": "AAAA"});
    let events = inputs(Some("gpt-live-1"), &[audio]);
    assert!(matches!(&events[0], Input::Session(s) if s.model.as_deref() == Some("gpt-live-1")));
    let frames = sent(&events).expect("written");
    assert_eq!(
        frames[0],
        json!({"type": "session.start", "session": {"model": "gpt-live-1"}})
    );
    assert_eq!(frames[1]["type"], "session.input_audio.append");

    let fault = sent(&inputs(None, &[json!({"type": "session.close"})])).expect_err("no model");
    assert!(fault.is(FaultKind::Client), "{fault}");
}

#[test]
fn another_formats_session_starts_it_with_what_the_live_api_takes() {
    let mut s = session("gpt-live-1");
    s.purpose = Some(Purpose::Conversation);
    s.instructions = Some("Be brief.".into());
    s.voice = Some("marin".into());
    s.output = Some(Modality::Audio);
    s.input_audio = Some(AudioFormat::pcm16(16_000));
    s.output_audio = Some(AudioFormat::pcm16(24_000));
    s.turns = Some(Turns::Server(Vad::default()));
    (s.input_transcription, s.output_transcription) = (Some(true), Some(true));
    s.tools = Some(Vec::new());
    let audio = |format: AudioFormat, bytes: usize| Input::Audio {
        audio: Audio {
            data: Bytes::from(vec![0; bytes]),
            format,
        },
        native: None,
    };
    let frames = sent(&[
        Input::Session(s),
        audio(AudioFormat::pcm16(16_000), 320),
        audio(AudioFormat::pcm16(48_000), 960),
    ])
    .expect("written");
    // Its audio is the client's, which it takes; the other direction is
    // converted by the client's writer.
    assert_eq!(
        frames[0],
        json!({"type": "session.start", "session": {"model": "gpt-live-1",
            "instructions": "Be brief.", "audio": {"output": {"voice": "marin"},
                "format": {"type": "audio/pcm", "rate": 16000}}}})
    );
    let bytes = |v: &Value| crate::json::unbase64(&v["audio"]).expect("base64").len();
    assert_eq!(bytes(&frames[1]), 320);
    assert!(bytes(&frames[2]) <= 320, "resampled to 16 kHz");

    // A format it does not take is converted to its own.
    let mut s = session("gpt-live-1");
    s.input_audio = Some(AudioFormat::pcm16(48_000));
    let frames = sent(&[Input::Session(s)]).expect("written");
    assert_eq!(frames[0]["session"].get("audio"), None);
}

#[test]
fn what_the_live_api_has_none_of_is_refused() {
    let refused = |s: Session| sent(&[Input::Session(s)]).expect_err("refused");
    let with = |f: fn(&mut Session)| {
        let mut s = session("gpt-live-1");
        f(&mut s);
        s
    };
    for s in [
        with(|s| s.purpose = Some(Purpose::Transcription)),
        with(|s| s.output = Some(Modality::Text)),
        with(|s| s.turns = Some(Turns::Manual)),
        with(|s| s.tools = Some(vec![Tool::new("f", json!({}))])),
        with(|s| s.max_output_tokens = Some(100)),
    ] {
        let fault = refused(s);
        assert!(fault.is(FaultKind::Client), "{fault}");
        assert_eq!(fault.get::<CloseCode>(), Some(&CloseCode(1008)));
    }
    let start = Input::Session(session("gpt-live-1"));
    for event in [
        Input::Commit { native: None },
        Input::Message {
            message: Message::new(Role::User, "hi"),
            native: None,
        },
        Input::ToolResult(ToolResult::new("c1", "{}")),
        Input::Respond { native: None },
        Input::Cancel { native: None },
    ] {
        let fault = sent(&[start.clone(), event.clone()]).expect_err("refused");
        assert_eq!(
            fault.get::<CloseCode>(),
            Some(&CloseCode(1008)),
            "{event:?}"
        );
    }
}

#[test]
fn a_session_is_started_once_and_then_only_acknowledged() {
    let (say, mut said) = spec::say();
    let mut provider = input::Provider::new(None, None, say);
    let mut frames = Vec::new();
    let start = Input::Session(session("gpt-live-1"));
    for event in [&start, &start] {
        provider.write(event, &mut frames).expect("written");
    }
    assert_eq!(
        frames.len(),
        1,
        "the update that changes nothing is no frame"
    );
    let acknowledged = said.try_recv().expect("said");
    assert!(matches!(acknowledged, Output::Updated(s) if s.model.as_deref() == Some("gpt-live-1")));
    let mut changed = Session::default();
    changed.instructions = Some("Be brief.".into());
    let fault = provider
        .write(&Input::Session(changed), &mut frames)
        .expect_err("refused");
    assert_eq!(fault.get::<CloseCode>(), Some(&CloseCode(1008)));
    assert!(
        fault
            .message()
            .ends_with("this update changes `instructions`")
    );
}

#[test]
fn every_server_event_is_written_back_as_it_came() {
    let snapshot = json!({"id": "live_1", "expires_at": 1794190905, "model": "gpt-live-1",
        "instructions": "", "status": "active", "delegation": {"type": "client"},
        "audio": {"output": {"voice": "marin"}, "format": {"type": "audio/pcm", "rate": 24000}},
        "input": []});
    let frames = [
        json!({"type": "session.started", "event_id": "s1", "session": snapshot}),
        json!({"type": "session.output_audio.delta", "delta": "AAEC"}),
        json!({"type": "session.input_transcript.delta", "event_id": "s2", "delta": "Hi",
            "start_ms": 0, "end_ms": 400}),
        json!({"type": "session.output_transcript.delta", "event_id": "s3",
            "delta": "Hello", "start_ms": 500, "end_ms": 900}),
        json!({"type": "session.delegation.created", "event_id": "s4", "offset_ms": 900,
            "delegation": {"id": "d1", "target": "client", "type": "delegation"}}),
        json!({"type": "session.usage.updated", "event_id": "s5", "usage": {"seconds": 1.5},
            "context_window": {"usage_ratio": 0.1}}),
        json!({"type": "error", "event_id": "s6", "error": {"type": "invalid_request_error",
            "code": "unknown_parameter", "message": "Unknown parameter.",
            "param": "session.voice"}}),
        json!({"type": "info", "event_id": "s7", "code": "c", "message": "m"}),
        json!({"type": "session.usage.updated", "event_id": "s8", "usage": {"seconds": 3.0}}),
        json!({"type": "session.closed", "event_id": "s9", "reason": "close_requested",
            "session": snapshot, "usage": {"seconds": 3.0}}),
    ];
    let events = outputs(&frames);
    assert!(matches!(&events[0], Output::Updated(s)
        if s.model.as_deref() == Some("gpt-live-1")
            && s.output_audio == Some(AudioFormat::pcm16(24_000))));
    assert!(matches!(&events[1], Output::Audio { audio, native: None }
        if audio.data[..] == [0, 1, 2]));
    assert!(
        matches!(&events[2], Output::InputTranscript { text, complete: false, .. }
        if text == "Hi")
    );
    assert!(matches!(&events[3], Output::Transcript { text, .. } if text == "Hello"));
    // Its usage is billed beside the event, which stays native.
    assert!(matches!(&events[5], Output::Seconds(_)));
    assert!(matches!(&events[6], Output::Native(_)));
    assert!(
        matches!(&events[7], Output::Error { cause: Cause::Invalid, code: Some(code), .. }
        if code == "unknown_parameter")
    );
    // Its usage is the session's whole: each says the seconds beyond the last.
    let billed: Vec<f64> = events
        .iter()
        .filter_map(|e| match e {
            Output::Seconds(s) => Some(*s),
            _ => None,
        })
        .collect();
    assert_eq!(billed, [1.5, 1.5]);
    assert_eq!(heard(&events).expect("written"), frames);
}

#[test]
fn another_formats_session_is_told_as_the_live_api_tells_it() {
    let mut opened = session("gpt-realtime");
    opened.output_audio = Some(AudioFormat::MULAW);
    let pcm = Audio {
        data: Bytes::from(vec![0; 480]),
        format: AudioFormat::pcm16(24_000),
    };
    let events = [
        Output::Opened(opened.clone()),
        Output::Updated(opened),
        Output::SpeechStarted {
            at_ms: Some(0),
            native: None,
        },
        Output::InputTranscript {
            text: "Hi".into(),
            complete: false,
            native: None,
        },
        Output::InputTranscript {
            text: "Hi".into(),
            complete: true,
            native: None,
        },
        Output::InputTranscript {
            text: "Bye".into(),
            complete: true,
            native: None,
        },
        Output::ResponseStart {
            id: None,
            native: None,
        },
        Output::Audio {
            audio: pcm,
            native: None,
        },
        Output::Transcript {
            text: "Hello".into(),
            native: None,
        },
        Output::Usage(Usage::default()),
        Output::ResponseDone {
            status: Status::Completed,
            native: None,
        },
        Output::Error {
            cause: Cause::Provider,
            code: Some("500".into()),
            message: "Oops.".into(),
            native: None,
        },
        Output::native(
            "openai-realtime",
            json!({"type": "conversation.item.added"}),
        ),
    ];
    let frames = heard(&events).expect("written");
    let types: Vec<&str> = frames.iter().filter_map(|f| f["type"].as_str()).collect();
    assert_eq!(
        types,
        [
            "session.started",
            "session.updated",
            "session.input_transcript.delta",
            "session.input_transcript.delta",
            "session.output_audio.delta",
            "session.output_transcript.delta",
            "error",
        ]
    );
    assert_eq!(frames[0]["session"]["model"], "gpt-realtime");
    assert_eq!(frames[0]["session"]["status"], "active");
    assert!(frames[0]["session"]["id"].is_string());
    for f in &frames {
        let audio = f["type"] == "session.output_audio.delta";
        assert_eq!(f["event_id"].is_string(), !audio, "{f}");
    }
    assert_eq!(
        frames[3]["delta"], "Bye",
        "a whole transcript of nothing heard"
    );
    let audio = crate::json::unbase64(&frames[4]["delta"]).expect("base64");
    // 10 ms of G.711 at 8 kHz, as its session says, less what the
    // resampler holds.
    assert!((60..=80).contains(&audio.len()), "{}", audio.len());
    assert_eq!(
        frames[6]["error"],
        json!({"type": "server_error", "message": "Oops."})
    );

    for refused in [
        Output::Text {
            text: "x".into(),
            native: None,
        },
        Output::ToolCall {
            id: "c1".into(),
            name: "f".into(),
            arguments: "{}".into(),
            native: None,
        },
    ] {
        let fault = heard(&[refused]).expect_err("refused");
        assert!(fault.is(FaultKind::Provider), "{fault}");
    }
}

#[test]
fn another_formats_natives_are_dropped_at_the_provider() {
    let mut s = session("gpt-live-1");
    s.native = Some(NativeFields::new(
        "openai-realtime",
        json!({"session": {"tracing": "auto"}})
            .as_object()
            .cloned()
            .expect("an object"),
    ));
    let frames = sent(&[
        Input::Session(s),
        Input::native(
            "gemini-live",
            json!({"realtimeInput": {"audioStreamEnd": true}}),
        ),
    ])
    .expect("written");
    assert_eq!(
        frames,
        [json!({"type": "session.start", "session": {"model": "gpt-live-1"}})]
    );
    assert_eq!(NAME, "openai-live");
}
