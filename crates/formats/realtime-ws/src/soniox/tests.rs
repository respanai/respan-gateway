use respan_gateway_core::{Fault, NativeFields};
use respan_gateway_http::{Decoder, Encoder, bytes::Bytes};
use respan_gateway_realtime::{Audio, AudioFormat, Input, Output, Session};
use serde_json::{Value, json};

use super::{NAME, input, output};

fn session(native: Value) -> Input {
    let mut session = Session::default();
    session.native = Some(NativeFields::new(
        NAME,
        native.as_object().expect("object").clone(),
    ));
    Input::Session(session)
}

fn read(provider: &mut output::Provider, value: Value, events: &mut Vec<Output>) {
    provider
        .read(Bytes::from(value.to_string()), events)
        .expect("valid response");
}

#[test]
fn configuration_is_fixed_and_a_routed_model_wins() {
    let mut provider = input::Provider::new(Some("stt-rt-v5".into()));
    let mut frames = Vec::new();
    let mut config = session(
        json!({"translation":{"type":"one_way","target_language":"es"},"language_hints":["en"]}),
    );
    if let Input::Session(session) = &mut config {
        session.model = Some("public-alias".into());
    }
    provider.write(&config, &mut frames).expect("configure");
    let sent: Value = serde_json::from_slice(&frames[0]).expect("JSON");
    assert_eq!(
        sent,
        json!({"model":"stt-rt-v5","audio_format":"pcm_s16le","sample_rate":16000,"num_channels":1,"translation":{"type":"one_way","target_language":"es"},"language_hints":["en"]})
    );
    provider
        .write(&Input::Session(Session::default()), &mut frames)
        .expect("same configuration");
    assert_eq!(frames.len(), 1);
    provider
        .write(
            &session(json!({"translation":{"type":"two_way","language_a":"en","language_b":"es"}})),
            &mut frames,
        )
        .expect_err("immutable configuration");
    provider
        .write(
            &session(json!({"api_key":"must-not-be-forwarded"})),
            &mut frames,
        )
        .expect_err("no body credentials");
    for event in [
        Input::Cancel { native: None },
        Input::Respond { native: None },
    ] {
        assert_eq!(
            provider
                .write(&event, &mut frames)
                .expect_err("not a conversation")
                .code(),
            "bad_request"
        );
    }
}

#[test]
fn base64_audio_finalization_and_finish_have_distinct_frames() {
    let mut provider = input::Provider::new(Some("stt-rt-v5".into()));
    let mut frames = Vec::new();
    provider
        .write(
            &Input::Audio {
                audio: Audio {
                    data: Bytes::from_static(&[1, 2, 3, 4]),
                    format: AudioFormat::pcm16(16000),
                },
                native: None,
            },
            &mut frames,
        )
        .expect("audio");
    provider
        .write(&Input::Commit { native: None }, &mut frames)
        .expect("finalize");
    provider
        .write(
            &Input::native(NAME, json!({"type":"keepalive"})),
            &mut frames,
        )
        .expect("keepalive");
    provider
        .write(&Input::native(NAME, ""), &mut frames)
        .expect("end audio");
    provider.end(Ok(()), &mut frames).expect("no duplicate end");
    assert_eq!(
        &frames[1..],
        [
            Bytes::from_static(b"AQIDBA=="),
            Bytes::from_static(br#"{"type":"finalize"}"#),
            Bytes::from_static(br#"{"type":"keepalive"}"#),
            Bytes::new()
        ]
    );
    provider
        .write(&Input::Commit { native: None }, &mut frames)
        .expect_err("already ended");
    let mut provider = input::Provider::new(Some("stt-rt-v5".into()));
    let mut cancelled = Vec::new();
    provider
        .write(&session(json!({})), &mut cancelled)
        .expect("start");
    provider
        .end(Err(Fault::abandoned()), &mut cancelled)
        .expect_err("cancelled");
    assert_eq!(
        cancelled.len(),
        1,
        "cancellation must not send normal finish"
    );
    provider.end(Ok(()), &mut cancelled).expect("normal end");
    assert_eq!(cancelled.last(), Some(&Bytes::new()));
}

#[test]
fn native_clients_send_configuration_then_base64_or_controls() {
    let mut client = input::Client::new(Some("routed".into()));
    let mut events = Vec::new();
    let config = json!({"model":"stt-rt-v5","audio_format":"auto","translation":{"type":"two_way","language_a":"en","language_b":"es"}});
    client
        .read(Bytes::from(config.to_string()), &mut events)
        .expect("config");
    for frame in [
        "AAAA",
        "1234",
        r#"{"type":"finalize"}"#,
        r#"{"type":"keepalive"}"#,
        "",
    ] {
        client
            .read(Bytes::from(frame.to_owned()), &mut events)
            .expect("frame");
    }
    let Input::Session(session) = &events[0] else {
        panic!("session");
    };
    assert_eq!(session.model.as_deref(), Some("stt-rt-v5"));
    assert_eq!(session.input_audio, None);
    assert!(matches!(&events[2], Input::Native(value) if *value.value == "1234"));
    client
        .read(Bytes::from_static(b"AAAA"), &mut events)
        .expect_err("ended");
    let mut client = input::Client::new(None);
    client
        .read(
            Bytes::from_static(br#"{"api_key":"private","model":"m","audio_format":"auto"}"#),
            &mut events,
        )
        .expect_err("body key");
    assert_eq!(events.len(), 6);
}

#[test]
fn pcm_clients_decode_audio_and_reject_a_second_configuration() {
    let mut client = input::Client::new(None);
    let mut events = Vec::new();
    let config = Bytes::from_static(
        br#"{"model":"stt-rt-v5","audio_format":"pcm_s16le","sample_rate":16000,"num_channels":1}"#,
    );
    client.read(config.clone(), &mut events).expect("config");
    client
        .read(Bytes::from_static(b"AAAA"), &mut events)
        .expect("audio");
    assert!(
        matches!(&events[1], Input::Audio { audio, .. } if audio.format == AudioFormat::pcm16(16000) && audio.data.len() == 3)
    );
    client.read(config, &mut events).expect_err("once");
    client
        .read(Bytes::from_static(b"not base64"), &mut events)
        .expect_err("invalid audio");
    client
        .read(Bytes::from_static(br#" {"type":"keepalive"}"#), &mut events)
        .expect("whitespace is valid JSON");
    client
        .read(
            Bytes::from_static(br#"{"type":"keepalive","api_key":"private"}"#),
            &mut events,
        )
        .expect_err("no body keys in controls");
}

#[test]
fn revisions_and_translations_are_native_and_only_final_source_is_appended() {
    let mut provider = output::Provider::default();
    let mut events = Vec::new();
    let frames = [
        json!({"tokens":[{"text":"wrong","is_final":false}],"total_audio_proc_ms":100}),
        json!({"tokens":[{"text":"Hello","is_final":true,"translation_status":"original","language":"en","speaker":"1","start_ms":0,"end_ms":100},{"text":" hola","is_final":false,"translation_status":"translation"}]}),
        json!({"tokens":[{"text":" world","is_final":true},{"text":"Hola mundo","is_final":true,"translation_status":"translation","language":"es","source_language":"en"},{"text":"<fin>","is_final":true}]}),
        json!({"tokens":[],"finished":true,"final_audio_proc_ms":200,"total_audio_proc_ms":200}),
    ];
    for frame in &frames {
        read(&mut provider, frame.clone(), &mut events);
    }
    let transcripts: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            Output::InputTranscript { text, complete, .. } => Some((text.as_str(), *complete)),
            _ => None,
        })
        .collect();
    assert_eq!(
        transcripts,
        [("Hello", false), (" world", false), ("Hello world", true)]
    );
    assert!(!events.iter().any(|event| matches!(event, Output::Usage(_))));
    let mut client = output::Client::default();
    let mut written = Vec::new();
    for event in &events {
        client.write(event, &mut written).expect("native output");
    }
    let written: Vec<Value> = written
        .iter()
        .map(|frame| serde_json::from_slice(frame).expect("JSON"))
        .collect();
    assert_eq!(
        written, frames,
        "native response fields and revisions survive exactly once"
    );
    provider.end(Ok(()), &mut Vec::new()).expect("finished");
}

#[test]
fn endpoints_and_finished_complete_each_source_turn_once() {
    let mut provider = output::Provider::default();
    let mut events = Vec::new();
    read(
        &mut provider,
        json!({"tokens":[{"text":"one","is_final":true},{"text":"<end>","is_final":true},{"text":"two","is_final":true}],"finished":true}),
        &mut events,
    );
    let complete: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            Output::InputTranscript {
                text,
                complete: true,
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(complete, ["one", "two"]);
    provider
        .read(Bytes::from_static(br#"{"tokens":[]}"#), &mut events)
        .expect_err("after terminal response");
}

#[test]
fn errors_are_terminal_and_eof_without_a_terminal_response_is_truncated() {
    let mut provider = output::Provider::default();
    let mut events = Vec::new();
    let error = json!({"tokens":[],"error_code":429,"error_type":"limit_exceeded","error_message":"slow down","request_id":"r1"});
    read(&mut provider, error.clone(), &mut events);
    assert!(matches!(&events[1], Output::Error { code: Some(code), .. } if code == "429"));
    let fault = provider
        .end(Ok(()), &mut Vec::new())
        .expect_err("terminal error must fail the session lifecycle");
    assert_eq!(fault.code(), "limit_exceeded");
    assert_eq!(respan_gateway_http_faults::status(&fault).as_u16(), 429);
    let mut client = output::Client::default();
    let mut written = Vec::new();
    for event in &events {
        client.write(event, &mut written).expect("error relay");
    }
    assert_eq!(written.len(), 1);
    assert_eq!(
        serde_json::from_slice::<Value>(&written[0]).expect("JSON"),
        error
    );
    let mut provider = output::Provider::default();
    read(&mut provider, json!({"tokens":[]}), &mut Vec::new());
    assert_eq!(
        provider
            .end(Ok(()), &mut Vec::new())
            .expect_err("missing finished")
            .code(),
        "truncated"
    );
    for invalid in [
        json!([]),
        json!({}),
        json!({"tokens":[{"text":"missing final"}]}),
        json!({"tokens":[],"finished":"yes"}),
        json!({"error_code":200,"error_message":"not an error"}),
        json!({"error_code":401,"error_type":42,"error_message":"bad key"}),
        json!({"error_code":401}),
        json!({"error_code":"401","error_message":"bad key"}),
    ] {
        output::Provider::default()
            .read(Bytes::from(invalid.to_string()), &mut Vec::new())
            .expect_err("malformed provider response");
    }
}

#[test]
fn an_auth_error_is_relayed_once_and_fails_completion_with_its_status_and_code() {
    let mut provider = output::Provider::default();
    let mut events = Vec::new();
    let error = json!({"tokens":[],"error_code":401,"error_type":"unauthenticated","error_message":"invalid API key"});
    read(&mut provider, error.clone(), &mut events);
    let mut client = output::Client::default();
    let mut frames = Vec::new();
    for event in &events {
        client.write(event, &mut frames).expect("relay error");
    }
    assert_eq!(frames.len(), 1);
    assert_eq!(
        serde_json::from_slice::<Value>(&frames[0]).expect("JSON"),
        error
    );
    let fault = provider
        .end(Ok(()), &mut Vec::new())
        .expect_err("provider auth failed");
    assert_eq!(respan_gateway_http_faults::status(&fault).as_u16(), 401);
    assert_eq!(fault.code(), "unauthenticated");
    assert_eq!(fault.message(), "invalid API key");
}

#[test]
fn translation_output_cannot_be_silently_dropped_by_another_client_format() {
    let translated = Output::native(
        NAME,
        json!({"tokens":[{"text":"Hola","is_final":true,"translation_status":"translation"}]}),
    );
    assert!(crate::foreign::strip("openai-realtime", &translated).is_err());
    let revised = Output::native(NAME, json!({"tokens":[{"text":"Hi","is_final":false}]}));
    assert!(
        crate::foreign::strip("openai-realtime", &revised)
            .expect("source revisions stay native")
            .is_none()
    );
}

#[test]
fn another_providers_stable_transcript_is_written_once_and_revisions_are_refused() {
    let mut client = output::Client::default();
    let mut frames = Vec::new();
    let transcript = |text: &str, complete| Output::InputTranscript {
        text: text.into(),
        complete,
        native: None,
    };
    client
        .write(&transcript("Hello", false), &mut frames)
        .expect("delta");
    client
        .write(&transcript("Hello world", true), &mut frames)
        .expect("whole transcript");
    let frames: Vec<Value> = frames
        .iter()
        .map(|frame| serde_json::from_slice(frame).expect("JSON"))
        .collect();
    assert_eq!(
        frames,
        [
            json!({"tokens":[{"text":"Hello","is_final":true}]}),
            json!({"tokens":[{"text":" world","is_final":true},{"text":"<fin>","is_final":true}]})
        ]
    );
    client
        .write(&transcript("wrong", false), &mut Vec::new())
        .expect("delta");
    client
        .write(&transcript("correct", true), &mut Vec::new())
        .expect_err("cannot retract final Soniox tokens");
}

#[test]
fn soniox_path_is_a_transcription_and_belongs_to_this_format() {
    use super::SonioxRealtime;
    use respan_gateway_core::Format;
    use respan_gateway_http::{Body, ClientIo, http};
    let request = http::Request::get("/v1/transcribe-websocket?model=stt-rt-v5")
        .header("upgrade", "websocket")
        .body(Body::default())
        .expect("request");
    let io = ClientIo::accept_live(request, 1 << 20).0;
    assert!(Format::<ClientIo, Input, Output>::matches(
        &SonioxRealtime,
        &io
    ));
    assert!(!Format::<ClientIo, Input, Output>::matches(
        &crate::OpenAiRealtime,
        &io
    ));
    assert!(crate::session::transcribes_at(io.request().uri()));
}
