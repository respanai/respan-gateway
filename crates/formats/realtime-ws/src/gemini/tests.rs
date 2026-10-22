use respan_gateway_core::Fault;
use respan_gateway_core::FaultKind;
use respan_gateway_http::bytes::Bytes;
use respan_gateway_realtime::{
    AudioFormat, Cause, Input, Modality, Output, Purpose, Session, Status, Turns,
};
use respan_gateway_ws::CloseCode;
use serde_json::{Value, json};

use super::{Answering, Shared, input, output};
use crate::session;

fn inputs(messages: &[Value]) -> Result<Vec<Input>, Fault> {
    let mut client = input::Client::new(Answering::default());
    let mut events = Vec::new();
    for message in messages {
        client.read(Bytes::from(message.to_string()), &mut events)?;
    }
    Ok(events)
}

fn sent(shared: &Shared, events: &[Input]) -> Result<Vec<Value>, Fault> {
    let mut provider = input::Provider::new(shared.clone(), session::say().0);
    let mut messages = Vec::new();
    for event in events {
        provider.write(event, &mut messages)?;
    }
    Ok(messages
        .iter()
        .map(|m| serde_json::from_slice(m).expect("JSON"))
        .collect())
}

fn outputs(shared: &Shared, messages: &[Value]) -> Vec<Output> {
    let mut provider = output::Provider::new(shared.clone());
    let mut events = Vec::new();
    for message in messages {
        provider
            .read(Bytes::from(message.to_string()), &mut events)
            .expect("read");
    }
    events
}

fn heard(events: &[Output]) -> Result<Vec<Value>, Fault> {
    let mut client = output::Client::new(Answering::default());
    let mut messages = Vec::new();
    for event in events {
        client.write(event, &mut messages)?;
    }
    Ok(messages
        .iter()
        .map(|m| serde_json::from_slice(m).expect("JSON"))
        .collect())
}

const SETUP: &str = r#"{"setup": {"model": "models/gemini-live",
    "generationConfig": {"responseModalities": ["AUDIO"], "temperature": 0.4,
        "speechConfig": {"voiceConfig": {"prebuiltVoiceConfig": {"voiceName": "Kore"}}},
        "maxOutputTokens": 900},
    "systemInstruction": {"parts": [{"text": "Be brief."}]},
    "tools": [{"functionDeclarations": [{"name": "weather", "description": "The weather.",
        "parameters": {"type": "OBJECT"}, "behavior": "NON_BLOCKING"}]}],
    "realtimeInputConfig": {"automaticActivityDetection": {"silenceDurationMs": 500,
        "startOfSpeechSensitivity": "START_SENSITIVITY_LOW"},
        "activityHandling": "NO_INTERRUPTION", "turnCoverage": "TURN_INCLUDES_ALL_INPUT"},
    "inputAudioTranscription": {}, "outputAudioTranscription": {},
    "sessionResumption": {"handle": "h1"}}}"#;

#[test]
fn every_client_message_is_written_back_as_it_came() {
    let messages = [
        serde_json::from_str(SETUP).expect("JSON"),
        json!({"realtimeInput": {"audio": {"mimeType": "audio/pcm;rate=16000", "data": "AAEC"}}}),
        json!({"realtimeInput": {"text": "Hi there"}}),
        json!({"clientContent": {"turns": [{"role": "user", "parts": [{"text": "a"}]},
            {"role": "model", "parts": [{"text": "b"}, {"inlineData": {"mimeType": "image/png",
                "data": "AA=="}}]}], "turnComplete": true}}),
        json!({"clientContent": {"turns": [{"role": "user", "parts": [{"text": "seed"}]}]}}),
        json!({"clientContent": {"turns": [{"role": "user", "parts": [{"text": "one"}]}],
            "turnComplete": true}}),
        json!({"clientContent": {"turnComplete": true}}),
        json!({"toolResponse": {"functionResponses": [
            {"id": "c1", "name": "weather", "response": {"celsius": 21}},
            {"id": "c2", "response": {"ok": true}, "scheduling": "SILENT"}]}}),
        json!({"realtimeInput": {"audioStreamEnd": true}}),
        json!({"clientContent": {"turnComplete": false}}),
        json!({"toolResponse": {"functionResponses": [{"id": "c3", "response": "text"}]}}),
    ];
    let events = inputs(&messages).expect("written");
    let kinds: Vec<&str> = events.iter().map(kind).collect();
    assert_eq!(
        kinds,
        [
            "session",
            "audio",
            "message",
            "respond",
            "message",
            "message",
            "respond",
            "message",
            "message",
            "respond",
            "respond",
            "tool_result",
            "tool_result",
            "respond",
            "native",
            "native",
            "native"
        ]
    );
    let applied = Shared::default();
    assert_eq!(sent(&applied, &events).expect("written"), messages);
    let Some(session) = applied.lock().applied.clone() else {
        panic!("no setup")
    };
    assert_eq!(session.model.as_deref(), Some("gemini-live"));
    assert_eq!(session.output, Some(Modality::Audio));
    assert_eq!(session.voice.as_deref(), Some("Kore"));
    assert_eq!(session.instructions.as_deref(), Some("Be brief."));
    assert_eq!(session.max_output_tokens, Some(900));
    assert_eq!(session.tools.as_ref().map(Vec::len), Some(1));
    let Some(Turns::Server(vad)) = &session.turns else {
        panic!("{session:?}")
    };
    assert_eq!((vad.silence_ms, vad.interrupt), (Some(500), Some(false)));
    assert_eq!(
        (session.input_transcription, session.output_transcription),
        (Some(true), Some(true))
    );
    assert_eq!(session.output_audio, Some(AudioFormat::pcm16(24_000)));
    let Input::Audio { audio, .. } = &events[1] else {
        panic!("{events:?}")
    };
    assert_eq!(audio.format, AudioFormat::pcm16(16_000));
}

fn kind(event: &Input) -> &'static str {
    match event {
        Input::Session(_) => "session",
        Input::Audio { .. } => "audio",
        Input::Commit { .. } => "commit",
        Input::Message { .. } => "message",
        Input::ToolResult(_) => "tool_result",
        Input::Respond { .. } => "respond",
        _ => "native",
    }
}

#[test]
fn a_client_must_set_up_first_and_once_one_thing_a_message() {
    let setup: Value = json!({"setup": {"model": "models/m"}});
    let text = json!({"realtimeInput": {"text": "hi"}});
    for (messages, refused) in [
        (vec![text.clone()], "first"),
        (vec![setup.clone(), setup.clone()], "once"),
        (
            vec![
                setup.clone(),
                json!({"realtimeInput": {}, "toolResponse": {}}),
            ],
            "one",
        ),
        (vec![json!("setup")], "one"),
    ] {
        let fault = inputs(&messages).expect_err("refused");
        assert!(fault.is(FaultKind::Client), "{fault}");
        assert_eq!(
            fault.get::<CloseCode>(),
            Some(&CloseCode(1007)),
            "{refused}"
        );
    }
}

#[test]
fn a_client_ends_its_turns_with_manual_detection() {
    let messages = [
        json!({"setup": {"model": "models/m",
            "realtimeInputConfig": {"automaticActivityDetection": {"disabled": true}}}}),
        json!({"realtimeInput": {"activityStart": {}}}),
        json!({"realtimeInput": {"audio": {"mimeType": "audio/pcm;rate=16000", "data": "AAEC"}}}),
        json!({"realtimeInput": {"activityEnd": {}}}),
    ];
    let events = inputs(&messages).expect("read");
    let kinds: Vec<&str> = events.iter().map(kind).collect();
    assert_eq!(kinds, ["session", "native", "audio", "commit", "respond"]);
    assert_eq!(
        sent(&Shared::default(), &events).expect("written"),
        messages
    );
}

#[test]
fn a_session_is_set_up_first_and_then_only_acknowledged() {
    let shared = Shared::default();
    let respond = Input::Respond { native: None };
    assert!(sent(&shared, &[respond]).is_err());
    let events = inputs(&[json!({"setup": {"model": "models/m"}})]).expect("written");
    let (say, mut said) = session::say();
    let mut provider = input::Provider::new(shared, say);
    let mut frames = Vec::new();
    for event in [&events[0], &events[0]] {
        provider.write(event, &mut frames).expect("written");
    }
    assert_eq!(
        frames.len(),
        1,
        "the update that changes nothing is no frame"
    );
    let acknowledged = said.try_recv().expect("said");
    assert!(matches!(acknowledged, Output::Updated(s) if s.model.as_deref() == Some("m")));
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
fn every_server_message_is_written_back_as_it_came() {
    let applied = Shared::default();
    let usage = json!({"promptTokenCount": 40, "responseTokenCount": 12,
        "cachedContentTokenCount": 0, "totalTokenCount": 52,
        "promptTokensDetails": [{"modality": "TEXT", "tokenCount": 10},
            {"modality": "AUDIO", "tokenCount": 30}],
        "responseTokensDetails": [{"modality": "AUDIO", "tokenCount": 12}]});
    let messages = [
        json!({"setupComplete": {}}),
        json!({"serverContent": {"inputTranscription": {"text": "what time"}}}),
        json!({"serverContent": {"modelTurn": {"parts": [{"inlineData":
            {"mimeType": "audio/pcm;rate=24000", "data": "AAEC"}}]}}}),
        json!({"serverContent": {"outputTranscription": {"text": "Noon."}}}),
        json!({"serverContent": {"generationComplete": true}}),
        json!({"serverContent": {"turnComplete": true}, "usageMetadata": usage}),
        json!({"voiceActivity": {"type": "ACTIVITY_START", "audioOffset": "0.760s"}}),
        json!({"serverContent": {"modelTurn": {"parts": [{"text": "Once"}]}}}),
        json!({"serverContent": {"interrupted": true}}),
        json!({"serverContent": {"turnComplete": true}}),
        json!({"serverContent": {"modelTurn": {"parts": [{"text": "Let me check."}]}}}),
        json!({"toolCall": {"functionCalls": [{"id": "c1", "name": "weather",
            "args": {"city": "Paris"}}, {"id": "c2", "name": "time", "args": {}}]}}),
        json!({"toolCallCancellation": {"ids": ["c2"]}}),
        json!({"serverContent": {"modelTurn": {"parts": [{"text": "21."}]},
            "turnComplete": true, "turnCompleteReason": "MALFORMED_FUNCTION_CALL"}}),
        json!({"usageMetadata": {"promptTokenCount": 3}}),
        json!({"serverContent": {"interrupted": true}}),
        json!({"goAway": {"timeLeft": "10s"}}),
        json!({"sessionResumptionUpdate": {"newHandle": "h2", "resumable": true}}),
        // A barge-in: the interruption, then the voice activity that made it.
        json!({"serverContent": {"modelTurn": {"parts": [{"text": "Twice"}]}}}),
        json!({"serverContent": {"interrupted": true}}),
        json!({"voiceActivity": {"type": "ACTIVITY_START", "audioOffset": "8s"}}),
        json!({"serverContent": {"turnComplete": true}}),
        json!({"voiceActivity": {"type": "ACTIVITY_END", "audioOffset": "9.500s"}}),
    ];
    let events = outputs(&applied, &messages);
    let statuses: Vec<&Status> = events
        .iter()
        .filter_map(|e| match e {
            Output::ResponseDone { status, .. } => Some(status),
            _ => None,
        })
        .collect();
    assert_eq!(
        statuses,
        [
            &Status::Completed,
            &Status::Interrupted,
            &Status::Completed,
            &Status::Failed,
            &Status::Interrupted
        ]
    );
    let used = events.iter().find_map(|e| match e {
        Output::Usage(used) => Some(*used),
        _ => None,
    });
    let used = used.expect("usage");
    assert_eq!(
        (used.tokens.input_tokens, used.tokens.output_tokens),
        (40, 12)
    );
    assert_eq!(
        (used.input_audio_tokens, used.output_audio_tokens),
        (30, 12)
    );
    let started: Vec<Option<u64>> = events
        .iter()
        .filter_map(|e| match e {
            Output::SpeechStarted { at_ms, .. } => Some(*at_ms),
            _ => None,
        })
        .collect();
    assert_eq!(
        started,
        [Some(760), None, None],
        "the voice activity, the interruption after no response, and the barge-in once"
    );
    assert!(events.iter().any(|e| matches!(
        e,
        Output::SpeechStopped {
            at_ms: Some(9_500),
            ..
        }
    )));
    let mut written = heard(&events).expect("written");
    // The turn of a part and its end is written as two messages, saying the same.
    let ended = written.remove(14);
    assert_eq!(
        ended,
        json!({"serverContent": {"turnComplete": true,
        "turnCompleteReason": "MALFORMED_FUNCTION_CALL"}})
    );
    written[13]["serverContent"]["turnComplete"] = true.into();
    written[13]["serverContent"]["turnCompleteReason"] = "MALFORMED_FUNCTION_CALL".into();
    assert_eq!(written, messages);
}

#[test]
fn the_setup_written_is_the_session_its_completion_says() {
    let applied = Shared::default();
    let events = inputs(&[json!({"setup": {"model": "models/m"}})]).expect("written");
    sent(&applied, &events).expect("written");
    let events = outputs(&applied, &[json!({"setupComplete": {}})]);
    let [Output::Updated(session)] = events.as_slice() else {
        panic!("{events:?}")
    };
    assert_eq!(session.model.as_deref(), Some("m"));
}

#[test]
fn a_refusal_closes_a_live_client() {
    let error = Output::Error {
        cause: Cause::Invalid,
        code: None,
        message: "no".into(),
        native: None,
    };
    let fault = heard(&[error]).expect_err("refused");
    assert_eq!(fault.get::<CloseCode>(), Some(&CloseCode(1007)));
    let foreign = Output::native("openai-realtime", json!({"type": "rate_limits.updated"}));
    assert_eq!(heard(&[foreign]).expect("dropped"), Vec::<Value>::new());
    let mcp = Output::native(
        "openai-realtime",
        json!({"type": "mcp_list_tools.completed"}),
    );
    assert!(heard(&[mcp]).is_err());
}

#[test]
fn a_cancel_that_found_no_response_closes_no_live_client() {
    let error = Output::Error {
        cause: Cause::NothingToCancel,
        code: None,
        message: "Cancellation failed: no active response found".into(),
        native: None,
    };
    assert_eq!(heard(&[error]).expect("dropped"), Vec::<Value>::new());
}

#[test]
fn a_cancelled_turn_says_nothing_more_and_a_withdrawn_call_takes_no_answer() {
    let shared = Shared::default();
    let (say, mut said) = session::say();
    let mut provider = input::Provider::new(shared.clone(), say);
    let mut frames = Vec::new();
    let setup = inputs(&[json!({"setup": {"model": "models/m"}})]).expect("read");
    provider.write(&setup[0], &mut frames).expect("written");
    let part = json!({"serverContent": {"modelTurn": {"parts": [{"text": "Once"}]}}});
    let started = outputs(&shared, std::slice::from_ref(&part));
    assert!(
        matches!(started[0], Output::ResponseStart { .. }),
        "{started:?}"
    );
    provider
        .write(&Input::Cancel { native: None }, &mut frames)
        .expect("written");
    assert_eq!(frames.len(), 1, "the Live API has no cancel");
    let done = said.try_recv().expect("said");
    assert!(matches!(
        done,
        Output::ResponseDone {
            status: Status::Cancelled,
            ..
        }
    ));
    let end = json!({"serverContent": {"turnComplete": true},
        "usageMetadata": {"promptTokenCount": 3}});
    let rest = outputs(&shared, &[part.clone(), end]);
    assert!(
        matches!(rest[0], Output::Usage(_)),
        "metered still: {rest:?}"
    );
    let said = |e: &Output| matches!(e, Output::Text { .. } | Output::ResponseDone { .. });
    assert!(!rest.iter().any(said), "{rest:?}");
    let next = outputs(&shared, &[part]);
    assert!(matches!(next[0], Output::ResponseStart { .. }), "{next:?}");

    outputs(&shared, &[json!({"toolCallCancellation": {"ids": ["c1"]}})]);
    let answer = Input::ToolResult(respan_gateway_realtime::ToolResult::new("c1", "{}"));
    provider.write(&answer, &mut frames).expect("dropped");
    provider
        .write(&Input::Respond { native: None }, &mut frames)
        .expect("dropped");
    assert_eq!(
        frames.len(),
        1,
        "no answer to a withdrawn call, nor a response"
    );
}

#[test]
fn a_transcription_drops_the_answer_and_ends_the_users_transcript() {
    let shared = Shared::default();
    let mut session = Session::default();
    session.purpose = Some(Purpose::Transcription);
    session.input_transcription = Some(true);
    sent(&shared, &[Input::Session(session)]).expect("sent");
    let events = outputs(
        &shared,
        &[
            json!({"serverContent": {"inputTranscription": {"text": "Hel"}}}),
            json!({"serverContent": {"inputTranscription": {"text": "lo"}}}),
            json!({"serverContent": {"modelTurn": {"parts": [{"text": "Hi there"}]}}}),
            json!({"serverContent": {"outputTranscription": {"text": "Hi there"}}}),
            json!({"serverContent": {"turnComplete": true}}),
        ],
    );
    let said: Vec<_> = events
        .iter()
        .map(|e| match e {
            Output::InputTranscript { text, complete, .. } => (text.as_str(), *complete),
            other => panic!("not dropped: {other:?}"),
        })
        .collect();
    assert_eq!(said, [("Hel", false), ("lo", false), ("Hello", true)]);
}

#[test]
fn a_transcriber_ends_the_users_transcript_at_its_generations_end() {
    let shared = Shared::default();
    let mut session = Session::default();
    session.purpose = Some(Purpose::Transcription);
    sent(&shared, &[Input::Session(session)]).expect("sent");
    let events = outputs(
        &shared,
        &[
            json!({"serverContent": {"inputTranscription": {"text": "Hello there."}}}),
            json!({"serverContent": {"generationComplete": true}}),
        ],
    );
    let said: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Output::InputTranscript { text, complete, .. } => Some((text.as_str(), *complete)),
            _ => None,
        })
        .collect();
    assert_eq!(said, [("Hello there.", false), ("Hello there.", true)]);
}

#[test]
fn a_session_that_says_no_purpose_is_a_conversation() {
    // An OpenAI client's session opened by its first audio says none; its
    // next says it converses, which changes nothing.
    let shared = Shared::default();
    let mut conversation = Session::default();
    conversation.purpose = Some(Purpose::Conversation);
    let sessions = [
        Input::Session(Session::default()),
        Input::Session(conversation),
    ];
    sent(&shared, &sessions).expect("no change");
    let mut transcription = Session::default();
    transcription.purpose = Some(Purpose::Transcription);
    let shared = Shared::default();
    let sessions = [
        Input::Session(Session::default()),
        Input::Session(transcription),
    ];
    sent(&shared, &sessions).expect_err("a change of purpose");
}

/// The user's whole transcripts of a transcription told by `messages`.
fn transcribed(messages: &[Value]) -> Vec<String> {
    let shared = Shared::default();
    let mut session = Session::default();
    session.purpose = Some(Purpose::Transcription);
    sent(&shared, &[Input::Session(session)]).expect("sent");
    outputs(&shared, messages)
        .into_iter()
        .filter_map(|e| match e {
            Output::InputTranscript {
                text,
                complete: true,
                ..
            } => Some(text),
            _ => None,
        })
        .collect()
}

#[test]
fn an_interrupted_answer_leaves_the_users_turn_to_its_end() {
    let heard = |text: &str| json!({"serverContent": {"inputTranscription": {"text": text}}});
    let answer = json!({"serverContent": {"modelTurn": {"parts": [{"text": "Sure"}]}}});
    let transcripts = transcribed(&[
        heard("One."),
        answer.clone(),
        json!({"serverContent": {"interrupted": true}}),
        heard(" Two."),
        answer,
        json!({"serverContent": {"turnComplete": true}}),
    ]);
    assert_eq!(transcripts, ["One.", " Two."]);
}

#[test]
fn a_transcribers_generation_ends_its_users_transcript_whatever_it_carries() {
    let transcripts = transcribed(&[
        json!({"serverContent": {"inputTranscription": {"text": "Hello", "finished": false}}}),
        json!({"serverContent": {"inputTranscription": {"text": " there."}}}),
        json!({"serverContent": {"generationComplete": true}}),
    ]);
    assert_eq!(transcripts, ["Hello there."]);
}
