use respan_gateway_core::{Fault, FaultKind};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_realtime::{AudioFormat, Input, Modality, Output, Purpose, Session, Turns};

use serde_json::{Value, json};

use super::{input, output};
use crate::session as spec;

fn frames<T>(
    events: &[T],
    mut write: impl FnMut(&T, &mut Vec<Bytes>) -> Result<(), Fault>,
) -> Result<Vec<Value>, Fault> {
    let mut out = Vec::new();
    for event in events {
        write(event, &mut out)?;
    }
    Ok(out
        .iter()
        .map(|f| serde_json::from_slice(f).expect("JSON"))
        .collect())
}

fn read<T>(
    frames: &[Value],
    mut read: impl FnMut(Bytes, &mut Vec<T>) -> Result<(), Fault>,
) -> Vec<T> {
    let mut out = Vec::new();
    for frame in frames {
        read(Bytes::from(frame.to_string()), &mut out).expect("read");
    }
    out
}

/// What `events` are written as to a provider, its model `model`.
fn sent(model: Option<&str>, events: &[Input]) -> Result<Vec<Value>, Fault> {
    let mut provider = input::Provider::new(model.map(str::to_owned), spec::say().0);
    frames(events, |e, out| provider.write(e, out))
}

fn heard(events: &[Output]) -> Result<Vec<Value>, Fault> {
    let mut client = output::Client::new();
    frames(events, |e, out| client.write(e, out))
}

fn session(f: impl FnOnce(&mut Session)) -> Input {
    let mut s = Session::default();
    s.model = Some("gpt-live-1".into());
    f(&mut s);
    Input::Session(s)
}

#[test]
fn every_event_is_written_back_as_it_came() {
    let client = [
        json!({"type": "session.start", "event_id": "e1", "session": {"model": "gpt-live-1",
            "instructions": "Be brief.", "store": true, "delegation": {"type": "client"},
            "audio": {"format": {"type": "audio/pcmu", "rate": 8000}, "output": {"voice": "marin"}}}}),
        json!({"type": "session.input_audio.append", "audio": "AAEC"}),
        json!({"type": "session.update", "session": {"delegation": {"type": "client"}}}),
        json!({"type": "session.close"}),
    ];
    let mut reader = input::Client::new();
    let events = read(&client, |raw, out| reader.read(raw, out));
    let Input::Session(s) = &events[0] else {
        panic!("{events:?}")
    };
    assert_eq!(
        (s.voice.as_deref(), s.input_audio),
        (Some("marin"), Some(AudioFormat::MULAW))
    );
    assert!(matches!(&events[1], Input::Audio { audio, .. } if audio.format == AudioFormat::MULAW));
    assert_eq!(sent(None, &events).expect("written"), client);

    let snapshot = json!({"id": "live_1", "model": "gpt-live-1", "status": "active",
        "audio": {"format": {"type": "audio/pcm", "rate": 24000}}});
    let server = [
        json!({"type": "session.started", "event_id": "s1", "session": snapshot}),
        json!({"type": "session.output_audio.delta", "delta": "AAEC"}),
        json!({"type": "session.input_transcript.delta", "event_id": "s2", "delta": "Hi",
            "start_ms": 0, "end_ms": 400}),
        json!({"type": "session.output_transcript.delta", "event_id": "s3", "delta": "Hello"}),
        json!({"type": "session.usage.updated", "event_id": "s4", "usage": {"seconds": 1.5}}),
        json!({"type": "error", "event_id": "s5", "error": {"type": "invalid_request_error",
            "code": "unknown_parameter", "message": "No."}}),
        json!({"type": "session.closed", "event_id": "s6", "reason": "close_requested",
            "session": snapshot, "usage": {"seconds": 3.0}}),
    ];
    let mut reader = output::Provider::new();
    let events = read(&server, |raw, out| reader.read(raw, out));
    // Its usage is the session's whole: each says the seconds beyond the last.
    let billed: Vec<f64> = events
        .iter()
        .filter_map(|e| match e {
            Output::Seconds(s) => Some(*s),
            _ => None,
        })
        .collect();
    assert_eq!(billed, [1.5, 1.5]);
    assert!(matches!(&events[0], Output::Updated(_)));
    assert!(matches!(&events[6], Output::Error { code: Some(c), .. } if c == "unknown_parameter"));
    assert_eq!(heard(&events).expect("written"), server);
}

#[test]
fn a_start_names_the_model_routing_judged_and_takes_what_it_can() {
    let mut s = Session::default();
    s.model = Some("alias".into());
    s.instructions = Some("Be brief.".into());
    s.input_audio = Some(AudioFormat::pcm16(48_000));
    s.output_audio = Some(AudioFormat::pcm16(16_000));
    s.turns = Some(Turns::Server(Default::default()));
    s.input_transcription = Some(true);
    let frames = sent(Some("gpt-live-1"), &[Input::Session(s.clone())]).expect("written");
    assert_eq!(
        frames,
        [
            json!({"type": "session.start", "session": {"model": "gpt-live-1",
        "instructions": "Be brief.", "audio": {"format": {"type": "audio/pcm", "rate": 16000}}}})
        ]
    );
    assert_eq!(
        sent(None, &[Input::Session(s)]).expect("written")[0]["session"]["model"],
        "alias"
    );
}

#[test]
fn what_the_live_api_has_none_of_is_refused() {
    let start = session(|_| {});
    for events in [
        vec![session(|s| s.purpose = Some(Purpose::Transcription))],
        vec![session(|s| s.output = Some(Modality::Text))],
        vec![session(|s| s.turns = Some(Turns::Manual))],
        vec![session(|s| s.max_output_tokens = Some(1))],
        vec![session(|s| s.model = None)],
        vec![start.clone(), Input::Commit { native: None }],
        vec![start.clone(), Input::Respond { native: None }],
        vec![
            start.clone(),
            session(|s| s.instructions = Some("x".into())),
        ],
    ] {
        let fault = sent(None, &events).expect_err("refused");
        assert_eq!(fault.kind(), FaultKind::Client, "{events:?}");
    }
    // A later session that changes nothing is acknowledged.
    let (say, mut said) = spec::say();
    let mut provider = input::Provider::new(None, say);
    let written =
        frames(&[start.clone(), start], |e, out| provider.write(e, out)).expect("written");
    assert_eq!(written.len(), 1);
    assert!(matches!(said.try_recv(), Ok(Output::Updated(_))));
}

#[test]
fn another_formats_session_is_told_as_the_live_api_tells_it() {
    let mut opened = Session::default();
    opened.output_audio = Some(AudioFormat::MULAW);
    let events = [
        Output::Opened(opened.clone()),
        Output::Updated(opened),
        Output::ResponseStart {
            id: None,
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
        Output::Transcript {
            text: "Hello".into(),
            native: None,
        },
        Output::Error {
            cause: respan_gateway_realtime::Cause::Provider,
            code: Some("500".into()),
            message: "Oops.".into(),
            native: None,
        },
    ];
    let types: Vec<Value> = heard(&events)
        .expect("written")
        .iter()
        .map(|f| f["type"].clone())
        .collect();
    assert_eq!(
        types,
        [
            "session.started",
            "session.updated",
            "session.input_transcript.delta",
            "session.output_transcript.delta",
            "error"
        ]
    );
    let text = Output::Text {
        text: "x".into(),
        native: None,
    };
    assert!(heard(&[text]).is_err());
}
