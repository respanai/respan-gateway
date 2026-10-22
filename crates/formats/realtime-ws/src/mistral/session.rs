//! A session object as the kind's [`Session`], both ways.

use respan_gateway_core::Fault;
use respan_gateway_realtime::{AudioFormat, Encoding, Purpose, Session};
use serde_json::{Value, json};

use super::NAME;
use crate::json::{self, at, remove, set, string, take, text};

const MODEL: &[&str] = &["session", "model"];
const FORMAT: &[&str] = &["session", "audio_format"];

/// What `v`, an event less its type, says of its `session`; what is left of
/// `v` is the session's native. It always transcribes the user's audio,
/// and answers nothing.
pub(super) fn decode(mut v: Value) -> Session {
    let v = &mut v;
    let mut s = Session::default();
    s.purpose = Some(Purpose::Transcription);
    s.model = take(v, MODEL, string, text);
    s.input_audio = take_format(v);
    s.input_transcription = Some(true);
    s.native = json::native(NAME, std::mem::take(v));
    s
}

/// The `session` object of `s`, but its native, which is its event's;
/// `refuse`d where it asks for what Mistral does not do.
pub(super) fn encode(s: &Session, refuse: fn(&str) -> Fault) -> Result<Value, Fault> {
    let asks = [
        (
            s.purpose == Some(Purpose::Conversation),
            "a conversation, as it only transcribes,",
        ),
        (s.instructions.is_some(), "instructions"),
        (s.output.is_some(), "an output modality"),
        (s.voice.is_some(), "a voice"),
        (s.output_audio.is_some(), "output audio"),
        (s.tools.is_some(), "tools"),
        (s.tool_choice.is_some(), "a tool choice"),
        (s.max_output_tokens.is_some(), "an output limit"),
        (s.input_transcription == Some(false), "no transcription"),
    ];
    if let Some((_, what)) = asks.iter().find(|(asked, _)| *asked) {
        return Err(refuse(what));
    }
    let mut v = json!({});
    if let Some(format) = s.input_audio {
        let format = audio_format_json(format).ok_or_else(|| refuse("an audio format"))?;
        set(&mut v, &FORMAT[1..], format);
    }
    Ok(v)
}

/// An audio format: PCM 16-bit at its rate, or G.711 at 8 kHz.
fn audio_format(v: &Value) -> Option<AudioFormat> {
    let rate = u32::try_from(v["sample_rate"].as_u64()?).ok()?;
    match v["encoding"].as_str()? {
        "pcm_s16le" => Some(AudioFormat::pcm16(rate)),
        "pcm_mulaw" if rate == 8000 => Some(AudioFormat::MULAW),
        "pcm_alaw" if rate == 8000 => Some(AudioFormat::ALAW),
        _ => None,
    }
}

pub(super) fn audio_format_json(f: AudioFormat) -> Option<Value> {
    let named = |encoding| Some(json!({"encoding": encoding, "sample_rate": f.rate}));
    match f.encoding {
        Encoding::Pcm16 => named("pcm_s16le"),
        Encoding::Mulaw if f == AudioFormat::MULAW => named("pcm_mulaw"),
        Encoding::Alaw if f == AudioFormat::ALAW => named("pcm_alaw"),
        _ => None,
    }
}

/// The audio format of the session `v`: its encoding and rate are taken,
/// what else it says stays.
fn take_format(v: &mut Value) -> Option<AudioFormat> {
    let format = audio_format(at(v, FORMAT)?)?;
    for field in ["encoding", "sample_rate"] {
        remove(v, &[FORMAT[0], FORMAT[1], field]);
    }
    Some(format)
}
