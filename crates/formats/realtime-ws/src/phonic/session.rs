//! A Phonic `config` as the kind's [`Session`], both ways.

use respan_gateway_realtime::{AudioFormat, Encoding, Session, Tool, Turns};
use serde_json::{Value, json};

use super::NAME;
use crate::json::{self, set, string, take, text};

const MODEL: &[&str] = &["model"];
const INSTRUCTIONS: &[&str] = &["system_prompt"];
const VOICE: &[&str] = &["voice_id"];
const INPUT_FORMAT: &[&str] = &["input_format"];
const OUTPUT_FORMAT: &[&str] = &["output_format"];
const MANUAL: &[&str] = &["push_to_talk"];
const TOOLS: &[&str] = &["tools"];

/// What `v`, a `config` less its type, says of the session; what is left of
/// `v`, such as its agent, tools and turn taking, is the session's native.
/// Phonic always transcribes the user's audio and the model's.
pub(super) fn decode(mut v: Value) -> Session {
    let v = &mut v;
    let mut s = Session::default();
    s.model = take(v, MODEL, string, text);
    s.instructions = take(v, INSTRUCTIONS, string, text);
    s.voice = take(v, VOICE, string, text);
    s.input_audio = take(v, INPUT_FORMAT, format, |f| format_json(*f));
    s.output_audio = take(v, OUTPUT_FORMAT, format, |f| format_json(*f));
    (s.input_transcription, s.output_transcription) = (Some(true), Some(true));
    s.native = json::native(NAME, std::mem::take(v));
    s
}

/// The `config` of `s`, its audio formats those Phonic takes, but its
/// native, which is its event's: turns the client ends are its push to
/// talk, and each tool one its client runs, defined for the conversation.
pub(super) fn encode(s: &Session) -> Value {
    let mut v = json!({"type": "config"});
    let mut put = |path: &[&str], value: Option<Value>| {
        if let Some(value) = value {
            set(&mut v, path, value);
        }
    };
    put(MODEL, s.model.as_ref().and_then(text));
    put(INSTRUCTIONS, s.instructions.as_ref().and_then(text));
    put(VOICE, s.voice.as_ref().and_then(text));
    put(INPUT_FORMAT, s.input_audio.and_then(format_json));
    put(OUTPUT_FORMAT, s.output_audio.and_then(format_json));
    put(
        MANUAL,
        (s.turns == Some(Turns::Manual)).then(|| true.into()),
    );
    let tools = s.tools.as_ref().filter(|tools| !tools.is_empty());
    put(TOOLS, tools.map(|tools| tools.iter().map(tool).collect()));
    v
}

/// A tool the client runs, as Phonic defines one in a `config`: its
/// arguments always match the schema, as Phonic takes no other.
fn tool(tool: &Tool) -> Value {
    let function = json!({
        "name": tool.name,
        "description": tool.description.as_deref().unwrap_or_default(),
        "parameters": tool.parameters,
        "strict": true,
    });
    json!({"type": "custom_websocket", "tool_schema": {"type": "function", "function": function}})
}

/// Whether Phonic takes audio in `format`: 16-bit PCM at 8, 16, 24 or
/// 44.1 kHz, or μ-law at 8 kHz.
pub(super) fn takes(format: AudioFormat) -> bool {
    format_json(format).is_some()
}

fn format(v: &Value) -> Option<AudioFormat> {
    Some(match v.as_str()? {
        "pcm_44100" => AudioFormat::pcm16(44_100),
        "pcm_24000" => AudioFormat::pcm16(24_000),
        "pcm_16000" => AudioFormat::pcm16(16_000),
        "pcm_8000" => AudioFormat::pcm16(8_000),
        "mulaw_8000" => AudioFormat::MULAW,
        _ => return None,
    })
}

fn format_json(f: AudioFormat) -> Option<Value> {
    let name = match (f.encoding, f.rate) {
        (Encoding::Pcm16, 8_000 | 16_000 | 24_000 | 44_100) => format!("pcm_{}", f.rate),
        (Encoding::Mulaw, 8_000) => "mulaw_8000".into(),
        _ => return None,
    };
    Some(name.into())
}
