//! OpenAI Realtime's beta protocol, which providers that adopted it before
//! GA still speak: GA's events, spelled as the beta spelled them.

use respan_gateway_core::{Connection, Fault, Format, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::{Connect, ProviderIo, Reach, Upstream};
use respan_gateway_realtime::{Input, Output};
use respan_gateway_ws::WsTransport;
use serde_json::{Map, Value, json};

use super::{TRANSCRIPTION_QUERY, call};
use crate::json;
use crate::session::Spec;

const SPEC: Spec = Spec {
    name: "openai-realtime-beta",
    path: "/realtime",
    protocol: None,
    binary_text: false,
    transcribes: false,
};

/// The server events the beta named otherwise, by their beta and GA names.
const RENAMED: &[(&str, &str)] = &[
    ("response.audio.delta", "response.output_audio.delta"),
    ("response.audio.done", "response.output_audio.done"),
    (
        "response.audio_transcript.delta",
        "response.output_audio_transcript.delta",
    ),
    (
        "response.audio_transcript.done",
        "response.output_audio_transcript.done",
    ),
    ("response.text.delta", "response.output_text.delta"),
    ("response.text.done", "response.output_text.done"),
    ("conversation.item.created", "conversation.item.added"),
];

/// A session's or response's fields the beta kept at its top, by their
/// beta name and their GA path in it.
const MOVED: &[(&str, &[&str])] = &[
    ("voice", &["audio", "output", "voice"]),
    ("speed", &["audio", "output", "speed"]),
    ("input_audio_format", &["audio", "input", "format"]),
    ("output_audio_format", &["audio", "output", "format"]),
    ("turn_detection", &["audio", "input", "turn_detection"]),
    (
        "input_audio_transcription",
        &["audio", "input", "transcription"],
    ),
    (
        "input_audio_noise_reduction",
        &["audio", "input", "noise_reduction"],
    ),
    ("max_response_output_tokens", &["max_output_tokens"]),
];

/// The audio formats the beta named, by their beta name and GA object: its
/// PCM is at 24 kHz alone, the only rate GA's writer sends.
fn formats() -> [(&'static str, Value); 3] {
    [
        ("pcm16", json!({"type": "audio/pcm", "rate": 24_000})),
        ("g711_ulaw", json!({"type": "audio/pcmu"})),
        ("g711_alaw", json!({"type": "audio/pcma"})),
    ]
}

/// The content parts of a model's message the beta named otherwise, by
/// their beta and GA type.
const PARTS: &[(&str, &str)] = &[("text", "output_text"), ("audio", "output_audio")];

/// OpenAI Realtime's beta protocol, as a provider speaks it: a call to
/// `/realtime` after its base URL, with the model at `?model=` and a bearer
/// key. Its events are GA's but for their spelling: some server events'
/// names, a session's fields at its top (`voice`, `input_audio_format`,
/// `turn_detection`), its formats' names (`pcm16`), both modalities where
/// GA says audio, and a model's message parts' types (`text`, `audio`).
/// Each frame is respelled at the wire, so it reads and writes as
/// [`OpenAiRealtime`](super::OpenAiRealtime) does, and its natives are
/// OpenAI Realtime's. It is no client's format: GA serves `/realtime`. A
/// transcription session, which the beta opened otherwise, is refused,
/// `bad_request`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenAiRealtimeBeta;

impl OpenAiRealtimeBeta {
    /// The format's name, `openai-realtime-beta`.
    pub const NAME: &'static str = SPEC.name;
    /// The path it is sent to.
    pub const PATH: &'static str = SPEC.path;

    /// A transport to its provider at `base_url` ([`WsTransport::new`]).
    pub fn transport(&self, base_url: impl Into<String>) -> Result<WsTransport, Fault> {
        SPEC.transport(base_url)
    }
}

/// Its spec's: a bearer key, as a WebSocket session.
impl Upstream for OpenAiRealtimeBeta {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn reach(&self) -> Reach {
        Reach::Session {
            binary_text: SPEC.binary_text,
        }
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for OpenAiRealtimeBeta {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        let (query, mut provider, mut reader) = call(&io)?;
        if query == TRANSCRIPTION_QUERY {
            return Err(Fault::bad_request(
                "OpenAI Realtime's beta protocol takes no transcription session",
            ));
        }
        let write = Box::new(move |event: &Input, out: &mut Vec<Bytes>| {
            let start = out.len();
            provider.write(event, out)?;
            for frame in out.iter_mut().skip(start) {
                *frame = respelled(frame, to_beta)?;
            }
            Ok(())
        });
        let read =
            move |raw: Bytes, out: &mut Vec<Output>| reader.read(respelled(&raw, to_ga)?, out);
        Ok(SPEC.provider(io, Some(query), write, Box::new(read), None))
    }
}

/// `frame` respelled by `respell`; one that is not a JSON object as it was.
fn respelled(frame: &Bytes, respell: fn(&mut Map<String, Value>)) -> Result<Bytes, Fault> {
    match serde_json::from_slice::<Value>(frame) {
        Ok(Value::Object(mut event)) => {
            respell(&mut event);
            json::frame(&Value::Object(event))
        }
        _ => Ok(frame.clone()),
    }
}

/// A client's event, as GA's writer wrote it, spelled as the beta did.
fn to_beta(event: &mut Map<String, Value>) {
    if let Some(Value::Object(session)) = event.get_mut("session") {
        session.remove("type");
        fields_to_beta(session);
    }
    if let Some(Value::Object(response)) = event.get_mut("response") {
        fields_to_beta(response);
    }
    respell_parts(event, |t| {
        PARTS.iter().find(|(_, ga)| *ga == t).map(|(b, _)| *b)
    });
}

/// A provider's event, spelled as the beta did, as GA's reader reads it.
fn to_ga(event: &mut Map<String, Value>) {
    let renamed = event.get("type").and_then(Value::as_str).and_then(|t| {
        RENAMED
            .iter()
            .find(|(beta, _)| *beta == t)
            .map(|(_, ga)| *ga)
    });
    if let Some(ga) = renamed {
        event.insert("type".into(), ga.into());
    }
    if let Some(Value::Object(session)) = event.get_mut("session") {
        session.insert("type".into(), "realtime".into());
        fields_to_ga(session);
    }
    if let Some(Value::Object(response)) = event.get_mut("response") {
        fields_to_ga(response);
    }
    respell_parts(event, |t| {
        PARTS.iter().find(|(beta, _)| *beta == t).map(|(_, ga)| *ga)
    });
}

/// A session's or response's fields, as GA's writer wrote them, spelled as
/// the beta did.
fn fields_to_beta(object: &mut Map<String, Value>) {
    if let Some(modalities) = object.remove("output_modalities") {
        object.insert("modalities".into(), modalities_to_beta(modalities));
    }
    for (name, path) in MOVED {
        if let Some(value) = take_at(object, path) {
            object.insert((*name).into(), format_to_beta(value));
        }
    }
    drop_empty_audio(object);
}

/// A session's or response's fields, spelled as the beta did, as GA's
/// reader reads them.
fn fields_to_ga(object: &mut Map<String, Value>) {
    if let Some(modalities) = object.remove("modalities") {
        object.insert("output_modalities".into(), modalities_to_ga(modalities));
    }
    for (name, path) in MOVED {
        if let Some(value) = object.remove(*name) {
            put_at(object, path, format_to_ga(value));
        }
    }
}

/// GA's output modalities as the beta's: audio comes with its transcript.
fn modalities_to_beta(modalities: Value) -> Value {
    match modalities.as_array().map(Vec::as_slice) {
        Some([m]) if m == "audio" => json!(["text", "audio"]),
        _ => modalities,
    }
}

/// The beta's modalities as GA's output modalities: audio with its
/// transcript is audio.
fn modalities_to_ga(modalities: Value) -> Value {
    match modalities
        .as_array()
        .is_some_and(|m| m.iter().any(|m| m == "audio"))
    {
        true => json!(["audio"]),
        false => modalities,
    }
}

/// A moved field's value as the beta says it: an audio format by its name.
fn format_to_beta(value: Value) -> Value {
    let named = formats().into_iter().find(|(_, ga)| *ga == value);
    named.map_or(value, |(beta, _)| beta.into())
}

/// A moved field's value as GA says it: an audio format as its object.
fn format_to_ga(value: Value) -> Value {
    let named = formats().into_iter().find(|(beta, _)| value == *beta);
    named.map_or(value, |(_, ga)| ga)
}

/// The types of the model's message parts in `event`'s item, or its
/// response's output, respelled by `respell`.
fn respell_parts(event: &mut Map<String, Value>, respell: impl Fn(&str) -> Option<&'static str>) {
    if let Some(part) = event.get_mut("part") {
        respell_part(part, &respell);
    }
    if let Some(item) = event.get_mut("item") {
        respell_item(item, &respell);
    }
    let output = event.get_mut("response").and_then(|r| r.get_mut("output"));
    for item in output.and_then(Value::as_array_mut).into_iter().flatten() {
        respell_item(item, &respell);
    }
}

/// The types of `item`'s parts respelled by `respell`, if it is the
/// model's message.
fn respell_item(item: &mut Value, respell: &impl Fn(&str) -> Option<&'static str>) {
    if item["role"] != "assistant" {
        return;
    }
    let parts = item.get_mut("content").and_then(Value::as_array_mut);
    for part in parts.into_iter().flatten() {
        respell_part(part, respell);
    }
}

/// The type of `part` respelled by `respell`.
fn respell_part(part: &mut Value, respell: &impl Fn(&str) -> Option<&'static str>) {
    let respelled = part["type"].as_str().and_then(respell);
    if let (Some(t), Some(part)) = (respelled, part.as_object_mut()) {
        part.insert("type".into(), t.into());
    }
}

/// The value at `path` in `object`, taken out of it.
fn take_at(object: &mut Map<String, Value>, path: &[&str]) -> Option<Value> {
    let (last, parents) = path.split_last()?;
    let mut at = object;
    for name in parents {
        at = at.get_mut(*name)?.as_object_mut()?;
    }
    at.remove(*last)
}

/// `value` put at `path` in `object`, making the objects on the way.
fn put_at(object: &mut Map<String, Value>, path: &[&str], value: Value) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut at = object;
    for name in parents {
        let next = at.entry(*name).or_insert_with(|| json!({}));
        let Some(next) = next.as_object_mut() else {
            return;
        };
        at = next;
    }
    at.insert((*last).into(), value);
}

/// `object`'s `audio`, and its `input` and `output`, dropped where the
/// fields moved out of them left them empty.
fn drop_empty_audio(object: &mut Map<String, Value>) {
    if let Some(Value::Object(audio)) = object.get_mut("audio") {
        drop_if_empty(audio, "input");
        drop_if_empty(audio, "output");
    }
    drop_if_empty(object, "audio");
}

/// The field `name` of `object` dropped if it is an empty object.
fn drop_if_empty(object: &mut Map<String, Value>, name: &str) {
    if object
        .get(name)
        .and_then(Value::as_object)
        .is_some_and(Map::is_empty)
    {
        object.remove(name);
    }
}

#[cfg(test)]
mod tests {
    use super::super::{input, output};
    use super::*;

    fn heard(frame: Value) -> Value {
        let raw = respelled(&Bytes::from(frame.to_string()), to_ga).expect("translate");
        let mut events = Vec::new();
        output::Provider::default()
            .read(raw, &mut events)
            .expect("read");
        let mut client = output::Client::default();
        let mut frames = Vec::new();
        for event in events {
            client.write(&event, &mut frames).expect("write");
        }
        serde_json::from_slice(frames.last().expect("a frame")).expect("JSON")
    }

    #[test]
    fn a_beta_session_is_a_ga_realtime_session() {
        let frame = heard(json!({"type": "session.created", "session": {
            "id": "s1", "modalities": ["text", "audio"], "voice": "alloy",
            "input_audio_format": "pcm16", "output_audio_format": "g711_ulaw"
        }}));
        assert_eq!(frame["session"]["type"], "realtime");
        assert_eq!(frame["session"]["output_modalities"], json!(["audio"]));
        assert_eq!(frame["session"]["audio"]["output"]["voice"], "alloy");
        assert_eq!(
            frame["session"]["audio"]["output"]["format"]["type"],
            "audio/pcmu"
        );
    }

    #[test]
    fn beta_content_parts_and_response_metadata_reach_ga_clients() {
        for kind in ["response.content_part.added", "response.content_part.done"] {
            let frame = heard(json!({"type": kind, "part": {"type": "audio", "transcript": "hi"}}));
            assert_eq!(frame["part"]["type"], "output_audio");
        }
        let frame = heard(json!({"type": "response.done", "response": {
            "status": "completed", "modalities": ["text", "audio"],
            "voice": "alloy", "output_audio_format": "pcm16",
            "output": [{"role": "assistant", "content": [{"type": "text", "text": "hi"}]}]
        }}));
        assert_eq!(frame["response"]["output_modalities"], json!(["audio"]));
        assert_eq!(frame["response"]["audio"]["output"]["voice"], "alloy");
        assert_eq!(
            frame["response"]["output"][0]["content"][0]["type"],
            "output_text"
        );
    }

    #[test]
    fn a_response_audio_override_is_sent_in_beta_shape() {
        let mut client = input::Client::new(Some("model".into()), None);
        let mut events = Vec::new();
        let frame = json!({"type": "response.create", "response": {
            "output_modalities": ["audio"], "audio": {"output": {
                "voice": "alloy", "format": {"type": "audio/pcmu"}
            }}
        }});
        client
            .read(Bytes::from(frame.to_string()), &mut events)
            .expect("read");
        let mut provider = input::Provider::new("model".into(), false);
        let mut frames = Vec::new();
        for event in events {
            provider.write(&event, &mut frames).expect("write");
        }
        let frame = respelled(frames.last().expect("response"), to_beta).expect("translate");
        let frame: Value = serde_json::from_slice(&frame).expect("JSON");
        assert_eq!(frame["response"]["modalities"], json!(["text", "audio"]));
        assert_eq!(frame["response"]["output_audio_format"], "g711_ulaw");
        assert!(frame["response"].get("audio").is_none());
    }
}
