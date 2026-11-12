//! A Live session object as the kind's [`Session`], both ways.

use respan_gateway_core::Native;
use respan_gateway_http::native::Json;
use respan_gateway_realtime::{AudioFormat, Encoding, Session};
use serde_json::{Value, json};

use super::Key;
use crate::json::{set, string, take, text};

const MODEL: &[&str] = &["session", "model"];
const INSTRUCTIONS: &[&str] = &["session", "instructions"];
const VOICE: &[&str] = &["session", "audio", "output", "voice"];
const FORMAT: &[&str] = &["session", "audio", "format"];

/// What `v`, an event less its type, says of its `session`; what is left of
/// `v` is the session's native. Its one audio format is both ways', and a
/// custom voice, an object, stays native. The Live API always transcribes
/// the user's audio and the model's.
pub(super) fn decode(mut v: Value) -> Session {
    let v = &mut v;
    let mut s = Session::default();
    s.model = take(v, MODEL, string, text);
    s.instructions = take(v, INSTRUCTIONS, string, text);
    s.voice = take(v, VOICE, string, text);
    let format = take(v, FORMAT, format, |f| format_json(*f));
    (s.input_audio, s.output_audio) = (format, format);
    (s.input_transcription, s.output_transcription) = (Some(true), Some(true));
    s.native = Native::json::<Key>(std::mem::take(v));
    s
}

/// The `session` object of `s`, its audio in `format` if the Live API
/// takes it, but its native, which is its event's.
pub(super) fn encode(s: &Session, format: Option<AudioFormat>) -> Value {
    let mut v = json!({});
    let mut put = |path: &[&str], value: Option<Value>| {
        if let Some(value) = value {
            set(&mut v, &path[1..], value);
        }
    };
    put(MODEL, s.model.as_ref().and_then(text));
    put(INSTRUCTIONS, s.instructions.as_ref().and_then(text));
    put(VOICE, s.voice.as_ref().and_then(text));
    put(FORMAT, format.and_then(format_json));
    v
}

/// Whether the Live API takes audio in `format`: 16-bit PCM at 16 or 24
/// kHz, or G.711 at 8 kHz.
pub(super) fn takes(format: AudioFormat) -> bool {
    format_json(format).is_some()
}

fn format(v: &Value) -> Option<AudioFormat> {
    let rate = u32::try_from(v["rate"].as_u64()?).ok()?;
    let format = match v["type"].as_str()? {
        "audio/pcm" => AudioFormat::pcm16(rate),
        "audio/pcmu" => AudioFormat::MULAW,
        "audio/pcma" => AudioFormat::ALAW,
        _ => return None,
    };
    (format.rate == rate).then_some(format)
}

fn format_json(f: AudioFormat) -> Option<Value> {
    let kind = match (f.encoding, f.rate) {
        (Encoding::Pcm16, 16_000 | 24_000) => "audio/pcm",
        (Encoding::Mulaw, 8_000) => "audio/pcmu",
        (Encoding::Alaw, 8_000) => "audio/pcma",
        _ => return None,
    };
    Some(json!({"type": kind, "rate": f.rate}))
}
