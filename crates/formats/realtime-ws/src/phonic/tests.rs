use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use respan_gateway_core::Fault;
use respan_gateway_http::Decoder;
use respan_gateway_http::bytes::Bytes;
use respan_gateway_realtime::{
    Audio, AudioFormat, Input, Modality, Output, Purpose, Session, Status, Turns,
};
use respan_gateway_ws::CloseCode;
use serde_json::{Value, json};

use super::{Applied, Asked, DEFAULT_FORMAT, input, output};
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
    let mut provider = input::Provider::new(
        model.map(str::to_owned),
        true,
        Applied::default(),
        spec::say().0,
    );
    frames(events, |e, out| provider.write(e, out))
}

fn heard(asked: AudioFormat, events: &[Output]) -> Result<Vec<Value>, Fault> {
    let mut client = output::Client::new(Asked::new(Mutex::new(asked)));
    frames(events, |e, out| client.write(e, out))
}

/// A provider's reader whose clock is `now`, which a test moves on.
fn clocked(applied: Applied, now: &Arc<Mutex<Instant>>) -> output::Provider {
    let now = now.clone();
    output::Provider::new(applied, Box::new(move || *now.lock()))
}

fn session(f: impl FnOnce(&mut Session)) -> Input {
    let mut s = Session::default();
    s.model = Some("phonic-1".into());
    f(&mut s);
    Input::Session(s)
}

fn billed(events: &[Output]) -> Vec<f64> {
    events
        .iter()
        .filter_map(|e| match e {
            Output::Seconds(s) => Some(*s),
            _ => None,
        })
        .collect()
}

#[test]
fn every_event_is_written_back_as_it_came() {
    let client = [
        json!({"type": "config", "model": "phonic-1", "system_prompt": "Be brief.",
            "voice_id": "sabrina", "input_format": "mulaw_8000", "output_format": "pcm_24000",
            "turn_taking": "patient", "tools": ["natural_conversation_ending"]}),
        json!({"type": "audio_chunk", "audio": "AAEC"}),
        json!({"type": "say", "text": "One moment."}),
    ];
    let asked = Asked::new(Mutex::new(DEFAULT_FORMAT));
    let mut reader = input::Client::new(asked.clone());
    let events = read(&client, |raw, out| reader.read(raw, out));
    let Input::Session(s) = &events[0] else {
        panic!("{events:?}")
    };
    assert_eq!(
        (s.voice.as_deref(), s.input_audio),
        (Some("sabrina"), Some(AudioFormat::MULAW))
    );
    assert_eq!(*asked.lock(), AudioFormat::pcm16(24_000));
    assert!(matches!(&events[1], Input::Audio { audio, .. } if audio.format == AudioFormat::MULAW));
    assert_eq!(sent(None, &events).expect("written"), client);

    let server = [
        json!({"type": "conversation_created", "conversation_id": "conv_1"}),
        json!({"type": "ready_to_start_conversation"}),
        json!({"type": "user_started_speaking"}),
        json!({"type": "user_finished_speaking"}),
        json!({"type": "input_text", "text": "Hi", "language": "en"}),
        json!({"type": "input_text", "text": "there", "language": "en"}),
        json!({"type": "assistant_started_speaking"}),
        json!({"type": "audio_chunk", "audio": "AAEC", "text": "Hello"}),
        json!({"type": "interrupted_response", "text": "Hel"}),
        json!({"type": "assistant_finished_speaking"}),
        json!({"type": "error", "error": {"message": "No.", "code": "invalid_config"}}),
    ];
    let applied = Applied::default();
    let now = Arc::new(Mutex::new(Instant::now()));
    let mut provider = clocked(applied, &now);
    let events = read(&server, |raw, out| provider.decode(raw, out));
    assert!(matches!(&events[1], Output::Updated(_)));
    // The user's words come one at a time, a space before each but the first.
    assert!(matches!(&events[5], Output::InputTranscript { text, .. } if text == " there"));
    // A chunk's text is the model's transcript, told before its audio.
    assert!(matches!(&events[7], Output::Transcript { text, .. } if text == "Hello"));
    let interrupted = Status::Interrupted;
    assert!(matches!(&events[9], Output::ResponseDone { status, .. } if *status == interrupted));
    assert_eq!(heard(DEFAULT_FORMAT, &events).expect("written"), server);
}

#[test]
fn the_seconds_billed_are_the_conversations_from_its_creation() {
    let now = Arc::new(Mutex::new(Instant::now()));
    let mut provider = clocked(Applied::default(), &now);
    let mut out = Vec::new();
    let mut frame = |v: Value, out: &mut Vec<Output>| {
        provider
            .decode(Bytes::from(v.to_string()), out)
            .expect("read");
    };
    frame(json!({"type": "ready_to_start_conversation"}), &mut out);
    *now.lock() += Duration::from_secs(5);
    frame(
        json!({"type": "conversation_created", "conversation_id": "c"}),
        &mut out,
    );
    *now.lock() += Duration::from_millis(1500);
    frame(json!({"type": "user_started_speaking"}), &mut out);
    *now.lock() += Duration::from_secs(2);
    provider.end(Ok(()), &mut out).expect("ended");
    assert_eq!(billed(&out), [1.5, 2.0]);
}

#[test]
fn a_config_names_the_model_routing_judged_and_takes_what_it_can() {
    let mut s = Session::default();
    s.model = Some("alias".into());
    s.instructions = Some("Be brief.".into());
    s.input_audio = Some(AudioFormat::pcm16(48_000));
    s.output_audio = Some(AudioFormat::pcm16(16_000));
    s.turns = Some(Turns::Server(Default::default()));
    s.input_transcription = Some(true);
    let frames = sent(Some("phonic-1"), &[Input::Session(s.clone())]).expect("written");
    assert_eq!(
        frames,
        [
            json!({"type": "config", "model": "phonic-1", "system_prompt": "Be brief.",
            "output_format": "pcm_16000"})
        ]
    );
    assert_eq!(
        sent(None, &[Input::Session(s)]).expect("written")[0]["model"],
        "alias"
    );
}

#[test]
fn audio_goes_in_chunks_of_at_most_40_ms_in_a_format_phonic_takes() {
    // 100 ms of 44.1 kHz PCM, Phonic's own: 40 ms is 3,528 bytes.
    let audio = |format, bytes| Input::Audio {
        audio: Audio {
            data: Bytes::from(vec![0; bytes]),
            format,
        },
        native: None,
    };
    let chunks = |events: &[Input]| -> Vec<usize> {
        sent(None, events).expect("written")[1..]
            .iter()
            .map(|v| crate::json::unbase64(&v["audio"]).expect("base64").len())
            .collect()
    };
    let own = chunks(&[session(|_| {}), audio(DEFAULT_FORMAT, 8_820)]);
    assert_eq!(own, [3_528, 3_528, 1_764]);
    // 100 ms of 24 kHz PCM is converted to it first.
    let converted = chunks(&[session(|_| {}), audio(AudioFormat::pcm16(24_000), 4_800)]);
    assert!(
        converted.iter().all(|bytes| *bytes <= 3_528),
        "{converted:?}"
    );
}

#[test]
fn what_phonic_has_none_of_is_refused() {
    let start = session(|_| {});
    for events in [
        vec![session(|s| s.purpose = Some(Purpose::Transcription))],
        vec![session(|s| s.output = Some(Modality::Text))],
        vec![session(|s| s.turns = Some(Turns::Manual))],
        vec![session(|s| s.max_output_tokens = Some(1))],
        vec![start.clone(), Input::Commit { native: None }],
        vec![start.clone(), Input::Respond { native: None }],
        vec![
            start.clone(),
            session(|s| s.instructions = Some("x".into())),
        ],
    ] {
        let fault = sent(None, &events).expect_err("refused");
        assert_eq!(
            fault.get::<CloseCode>(),
            Some(&CloseCode(1008)),
            "{events:?}"
        );
    }
    // A later session that changes nothing is acknowledged.
    let (say, mut said) = spec::say();
    let mut provider = input::Provider::new(None, true, Applied::default(), say);
    let written =
        frames(&[start.clone(), start], |e, out| provider.write(e, out)).expect("written");
    assert_eq!(written.len(), 1);
    assert!(matches!(said.try_recv(), Ok(Output::Updated(_))));
}

#[test]
fn another_formats_session_is_told_as_phonic_tells_it() {
    let audio = Output::Audio {
        audio: Audio {
            data: Bytes::from_static(&[0, 1]),
            format: AudioFormat::pcm16(24_000),
        },
        native: None,
    };
    let events = [
        Output::Opened(Session::default()),
        Output::Updated(Session::default()),
        // Another format's parts of a word are one word.
        Output::InputTranscript {
            text: "H".into(),
            complete: false,
            native: None,
        },
        Output::InputTranscript {
            text: "i".into(),
            complete: false,
            native: None,
        },
        Output::InputTranscript {
            text: "Hi".into(),
            complete: true,
            native: None,
        },
        Output::Transcript {
            text: "Hel".into(),
            native: None,
        },
        Output::Transcript {
            text: "lo".into(),
            native: None,
        },
        audio,
        Output::Seconds(1.0),
        Output::ResponseDone {
            status: Status::Cancelled,
            native: None,
        },
    ];
    let written = heard(AudioFormat::pcm16(24_000), &events).expect("written");
    let types: Vec<&Value> = written.iter().map(|f| &f["type"]).collect();
    assert_eq!(
        types,
        [
            "ready_to_start_conversation",
            "input_text",
            "audio_chunk",
            "assistant_finished_speaking"
        ]
    );
    assert_eq!(written[1]["text"], "Hi");
    assert_eq!(written[2]["text"], "Hello");
    let text = Output::Text {
        text: "x".into(),
        native: None,
    };
    assert!(heard(DEFAULT_FORMAT, &[text]).is_err());
}
