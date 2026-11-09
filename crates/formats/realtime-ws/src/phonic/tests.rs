use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use respan_gateway_core::Fault;
use respan_gateway_http::Decoder;
use respan_gateway_http::bytes::Bytes;
use respan_gateway_realtime::{
    Audio, AudioFormat, Input, Modality, Output, Purpose, Session, Status,
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

fn heard(events: &[Output]) -> Result<Vec<Value>, Fault> {
    let mut client = output::Client::new(Asked::new(Mutex::new(DEFAULT_FORMAT)));
    frames(events, |e, out| client.write(e, out))
}

fn session(f: impl FnOnce(&mut Session)) -> Input {
    let mut s = Session::default();
    s.model = Some("phonic-1".into());
    f(&mut s);
    Input::Session(s)
}

fn audio(format: AudioFormat, bytes: usize) -> Audio {
    Audio {
        data: Bytes::from(vec![0; bytes]),
        format,
    }
}

fn words(text: &str, complete: bool) -> Output {
    Output::InputTranscript {
        text: text.into(),
        complete,
        native: None,
    }
}

#[test]
fn every_event_is_written_back_as_it_came() {
    let client = [
        json!({"type": "config", "model": "phonic-1", "system_prompt": "Be brief.",
            "voice_id": "sabrina", "input_format": "mulaw_8000", "output_format": "pcm_24000",
            "turn_taking": "patient"}),
        json!({"type": "audio_chunk", "audio": "AAEC"}),
        json!({"type": "say", "text": "One moment."}),
    ];
    let asked = Asked::new(Mutex::new(DEFAULT_FORMAT));
    let mut reader = input::Client::new(asked.clone());
    let events = read(&client, |raw, out| reader.read(raw, out));
    assert!(matches!(&events[1], Input::Audio { audio, .. } if audio.format == AudioFormat::MULAW));
    assert_eq!(*asked.lock(), AudioFormat::pcm16(24_000));
    assert_eq!(sent(None, &events).expect("written"), client);

    let server = [
        json!({"type": "ready_to_start_conversation"}),
        json!({"type": "user_started_speaking"}),
        json!({"type": "input_text", "text": "Hi", "language": "en"}),
        json!({"type": "input_text", "text": "there", "language": "en"}),
        json!({"type": "assistant_started_speaking"}),
        json!({"type": "audio_chunk", "audio": "AAEC", "text": "Hello"}),
        json!({"type": "interrupted_response", "text": "Hel"}),
        json!({"type": "error", "error": {"message": "No.", "code": "invalid_config"}}),
    ];
    let mut provider = output::Provider::new(Applied::default(), Box::new(Instant::now));
    let events = read(&server, |raw, out| provider.decode(raw, out));
    // The user's words come one at a time, a space before each but the first.
    assert!(matches!(&events[3], Output::InputTranscript { text, .. } if text == " there"));
    // A chunk's text is the model's transcript, told before its audio.
    assert!(matches!(&events[5], Output::Transcript { text, .. } if text == "Hello"));
    assert!(matches!(
        &events[7],
        Output::ResponseDone {
            status: Status::Interrupted,
            ..
        }
    ));
    assert_eq!(heard(&events).expect("written"), server);
}

#[test]
fn the_seconds_billed_are_the_conversations_from_its_creation() {
    let now = Arc::new(Mutex::new(Instant::now()));
    let clock = now.clone();
    let mut provider = output::Provider::new(Applied::default(), Box::new(move || *clock.lock()));
    let mut out = Vec::new();
    for (wait, kind) in [
        (0, "ready_to_start_conversation"),
        (5_000, "conversation_created"),
        (1_500, "user_started_speaking"),
    ] {
        *now.lock() += Duration::from_millis(wait);
        provider
            .decode(Bytes::from(json!({"type": kind}).to_string()), &mut out)
            .expect("read");
    }
    *now.lock() += Duration::from_secs(2);
    provider.end(Ok(()), &mut out).expect("ended");
    let billed: Vec<f64> = out
        .iter()
        .filter_map(|e| {
            if let Output::Seconds(s) = e {
                Some(*s)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(billed, [1.5, 2.0]);
}

#[test]
fn a_config_names_the_model_routing_judged_and_takes_what_it_can() {
    let s = session(|s| {
        s.model = Some("alias".into());
        s.instructions = Some("Be brief.".into());
        s.input_audio = Some(AudioFormat::pcm16(48_000));
        s.output_audio = Some(AudioFormat::pcm16(16_000));
        s.input_transcription = Some(true);
    });
    let written = sent(Some("phonic-1"), std::slice::from_ref(&s)).expect("written");
    let config = json!({"type": "config", "model": "phonic-1", "system_prompt": "Be brief.",
        "output_format": "pcm_16000"});
    assert_eq!(written, [config]);
    assert_eq!(sent(None, &[s]).expect("written")[0]["model"], "alias");
}

#[test]
fn audio_goes_in_chunks_of_at_most_40_ms() {
    // 100 ms of 44.1 kHz PCM, Phonic's own: 40 ms is 3,528 bytes.
    let chunk = Input::Audio {
        audio: audio(DEFAULT_FORMAT, 8_820),
        native: None,
    };
    let written = sent(None, &[session(|_| {}), chunk]).expect("written");
    let sizes: Vec<usize> = written[1..]
        .iter()
        .map(|v| crate::json::unbase64(&v["audio"]).expect("base64").len())
        .collect();
    assert_eq!(sizes, [3_528, 3_528, 1_764]);
}

#[test]
fn what_phonic_has_none_of_is_refused() {
    let start = session(|_| {});
    for events in [
        vec![session(|s| s.purpose = Some(Purpose::Transcription))],
        vec![session(|s| s.output = Some(Modality::Text))],
        vec![start.clone(), Input::Commit { native: None }],
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
    // A later session that changes nothing the client asked is
    // acknowledged, though another format's voice and its model were not
    // what Phonic was told.
    let (say, mut said) = spec::say();
    let mut provider =
        input::Provider::new(Some("phonic-1".into()), false, Applied::default(), say);
    let asked = session(|s| {
        s.model = Some("alias".into());
        s.voice = Some("alloy".into());
    });
    let written =
        frames(&[asked.clone(), asked], |e, out| provider.write(e, out)).expect("written");
    assert_eq!(written.len(), 1);
    assert!(matches!(said.try_recv(), Ok(Output::Updated(_))));
}

#[test]
fn another_formats_session_is_told_as_phonic_tells_it() {
    let transcript = |text: &str| Output::Transcript {
        text: text.into(),
        native: None,
    };
    let events = [
        Output::Opened(Session::default()),
        // Another format's parts of a word are one word.
        words("H", false),
        words("i", false),
        words("Hi", true),
        transcript("Hel"),
        transcript("lo"),
        Output::Audio {
            audio: audio(DEFAULT_FORMAT, 2),
            native: None,
        },
        Output::Seconds(1.0),
        // Told after its last audio: with an empty chunk at its turn's end.
        transcript(" there."),
        Output::ResponseDone {
            status: Status::Cancelled,
            native: None,
        },
    ];
    let written = heard(&events).expect("written");
    let types: Vec<&Value> = written.iter().map(|f| &f["type"]).collect();
    assert_eq!(
        types,
        [
            "ready_to_start_conversation",
            "input_text",
            "audio_chunk",
            "audio_chunk",
            "assistant_finished_speaking"
        ]
    );
    let texts: Vec<&Value> = written[1..4].iter().map(|f| &f["text"]).collect();
    assert_eq!(texts, ["Hi", "Hello", " there."]);
    assert_eq!(written[3]["audio"], "");
    assert!(
        heard(&[Output::Text {
            text: "x".into(),
            native: None
        }])
        .is_err()
    );
}
