//! A GA session object as the kind's [`Session`], both ways.

use respan_gateway_core::Fault;
use respan_gateway_realtime::{
    AudioFormat, Encoding, Modality, Purpose, Session, Tool, ToolChoice, Turns, Vad,
};
use serde_json::{Map, Value, json};

use super::NAME;
use crate::json::{self, at, merge, number, own, set, string, take, text};

const TYPE: &[&str] = &["session", "type"];
const MODEL: &[&str] = &["session", "model"];
const INSTRUCTIONS: &[&str] = &["session", "instructions"];
const MODALITIES: &[&str] = &["session", "output_modalities"];
const VOICE: &[&str] = &["session", "audio", "output", "voice"];
const INPUT_FORMAT: &[&str] = &["session", "audio", "input", "format"];
const OUTPUT_FORMAT: &[&str] = &["session", "audio", "output", "format"];
const TURN_DETECTION: &[&str] = &["session", "audio", "input", "turn_detection"];
const TRANSCRIPTION: &[&str] = &["session", "audio", "input", "transcription"];
const TOOLS: &[&str] = &["session", "tools"];
const TOOL_CHOICE: &[&str] = &["session", "tool_choice"];
const MAX_OUTPUT_TOKENS: &[&str] = &["session", "max_output_tokens"];

/// What `v`, an event less its type, says of its `session`; what is left of
/// `v` is the session's native. OpenAI always transcribes the model's
/// audio.
pub(super) fn decode(mut v: Value) -> Session {
    let v = &mut v;
    let mut s = Session::default();
    s.purpose = take(v, TYPE, purpose, |p| purpose_json(*p));
    s.model = take(v, MODEL, string, text);
    s.instructions = take(v, INSTRUCTIONS, string, text);
    s.output = take(v, MODALITIES, modality, |m| modality_json(*m));
    s.voice = take(v, VOICE, string, text);
    s.input_audio = take_format(v, INPUT_FORMAT);
    s.output_audio = take_format(v, OUTPUT_FORMAT);
    s.turns = take(v, TURN_DETECTION, turns, |t| turns_json(t).ok());
    // A transcription's settings, such as its model, stay where they are.
    s.input_transcription = take(
        v,
        TRANSCRIPTION,
        |t| t.is_null().then_some(false),
        |_| Some(Value::Null),
    )
    .or_else(|| at(v, TRANSCRIPTION)?.is_object().then_some(true));
    s.output_transcription = Some(true);
    s.tools = take(v, TOOLS, tools, |t| Some(tools_json(t)));
    s.tool_choice = take(v, TOOL_CHOICE, tool_choice, tool_choice_json);
    s.max_output_tokens = take(v, MAX_OUTPUT_TOKENS, Value::as_u64, number);
    s.native = json::native(NAME, std::mem::take(v));
    s
}

/// The `session` object of `s`, but its native, which is its event's;
/// refused by `refuse` where it holds another format's fields.
pub(super) fn encode(s: &Session, refuse: fn(&str) -> Fault) -> Result<Value, Fault> {
    let mut v = json!({});
    let mut put = |path: &[&str], value: Option<Value>| {
        if let Some(value) = value {
            set(&mut v, &path[1..], value);
        }
    };
    put(TYPE, s.purpose.and_then(purpose_json));
    put(MODEL, s.model.as_ref().and_then(text));
    put(INSTRUCTIONS, s.instructions.as_ref().and_then(text));
    put(MODALITIES, s.output.and_then(modality_json));
    put(VOICE, s.voice.as_ref().and_then(text));
    let formats = [
        (INPUT_FORMAT, s.input_audio),
        (OUTPUT_FORMAT, s.output_audio),
    ];
    for (path, format) in formats {
        let format = format.map(|f| audio_format_json(f).ok_or_else(|| refuse("an audio format")));
        put(path, format.transpose()?);
    }
    put(
        TURN_DETECTION,
        s.turns
            .as_ref()
            .map(|t| turns_json(t).map_err(refuse))
            .transpose()?,
    );
    put(
        TRANSCRIPTION,
        s.input_transcription.map(|on| match on {
            true => json!({}),
            false => Value::Null,
        }),
    );
    put(TOOLS, s.tools.as_ref().map(|t| tools_json(t)));
    put(
        TOOL_CHOICE,
        s.tool_choice.as_ref().and_then(tool_choice_json),
    );
    put(
        MAX_OUTPUT_TOKENS,
        s.max_output_tokens.as_ref().and_then(number),
    );
    Ok(v)
}

fn purpose(v: &Value) -> Option<Purpose> {
    match v.as_str()? {
        "realtime" => Some(Purpose::Conversation),
        "transcription" => Some(Purpose::Transcription),
        _ => None,
    }
}

fn purpose_json(p: Purpose) -> Option<Value> {
    match p {
        Purpose::Conversation => Some("realtime".into()),
        Purpose::Transcription => Some("transcription".into()),
        _ => None,
    }
}

fn modality(v: &Value) -> Option<Modality> {
    match v.as_array()?.as_slice() {
        [m] if m == "audio" => Some(Modality::Audio),
        [m] if m == "text" => Some(Modality::Text),
        _ => None,
    }
}

fn modality_json(m: Modality) -> Option<Value> {
    match m {
        Modality::Audio => Some(json!(["audio"])),
        Modality::Text => Some(json!(["text"])),
        _ => None,
    }
}

/// Whether OpenAI takes audio in `format`: 16-bit PCM at 24 kHz, or G.711.
pub(super) fn takes(format: AudioFormat) -> bool {
    [
        AudioFormat::pcm16(24_000),
        AudioFormat::MULAW,
        AudioFormat::ALAW,
    ]
    .contains(&format)
}

/// A GA audio format: `audio/pcm` at its rate, or G.711.
pub(super) fn audio_format(v: &Value) -> Option<AudioFormat> {
    match v["type"].as_str()? {
        "audio/pcm" => Some(AudioFormat::pcm16(v["rate"].as_u64()?.try_into().ok()?)),
        "audio/pcmu" => Some(AudioFormat::MULAW),
        "audio/pcma" => Some(AudioFormat::ALAW),
        _ => None,
    }
}

/// The audio format at `path` in `v`: its type, and a PCM one's rate, are
/// taken; what else it says stays.
fn take_format(v: &mut Value, path: &[&str]) -> Option<AudioFormat> {
    let format = audio_format(at(v, path)?)?;
    let field = |name| [path, &[name]].concat();
    take(v, &field("type"), string, text);
    if format.encoding == Encoding::Pcm16 {
        take(v, &field("rate"), |r| r.as_u64(), number);
    }
    Some(format)
}

fn audio_format_json(f: AudioFormat) -> Option<Value> {
    match f.encoding {
        Encoding::Pcm16 => Some(json!({"type": "audio/pcm", "rate": f.rate})),
        Encoding::Mulaw if f == AudioFormat::MULAW => Some(json!({"type": "audio/pcmu"})),
        Encoding::Alaw if f == AudioFormat::ALAW => Some(json!({"type": "audio/pcma"})),
        _ => None,
    }
}

/// Milliseconds, as a VAD's settings say them.
fn millis(v: &Value) -> Option<u32> {
    v.as_u64()?.try_into().ok()
}

fn turns(v: &Value) -> Option<Turns> {
    if v.is_null() {
        return Some(Turns::Manual);
    }
    let mut rest = v.clone();
    let rest = &mut rest;
    let mut vad = Vad::default();
    let flag = |v: &Value| v.as_bool();
    let bool_json = |b: &bool| Some(Value::Bool(*b));
    let millis_json = |n: &u32| Some(Value::from(*n));
    vad.silence_ms = take(rest, &["silence_duration_ms"], millis, millis_json);
    vad.prefix_ms = take(rest, &["prefix_padding_ms"], millis, millis_json);
    vad.respond = take(rest, &["create_response"], flag, bool_json);
    vad.interrupt = take(rest, &["interrupt_response"], flag, bool_json);
    vad.native = json::native(NAME, std::mem::take(rest));
    Some(Turns::Server(vad))
}

/// Turn detection as GA has it: none for manual turns, else server VAD, or
/// what type its native names; `Err` names what it cannot write.
fn turns_json(turns: &Turns) -> Result<Value, &'static str> {
    let vad = match turns {
        Turns::Manual => return Ok(Value::Null),
        Turns::Server(vad) => vad,
        _ => return Err("this turn detection"),
    };
    let mut v = Map::new();
    let mut put = |key: &str, value: Option<Value>| {
        if let Some(value) = value {
            v.insert(key.into(), value);
        }
    };
    put("silence_duration_ms", vad.silence_ms.map(Value::from));
    put("prefix_padding_ms", vad.prefix_ms.map(Value::from));
    put("create_response", vad.respond.map(Value::from));
    put("interrupt_response", vad.interrupt.map(Value::from));
    let mut v = Value::Object(v);
    merge(&mut v, own(&vad.native));
    if v.get("type").is_none() {
        v["type"] = "server_vad".into();
    }
    Ok(v)
}

fn tools(v: &Value) -> Option<Vec<Tool>> {
    v.as_array()?.iter().map(tool).collect()
}

/// A function tool; a tool of another type has no form in the kind.
fn tool(v: &Value) -> Option<Tool> {
    let mut rest = v.clone();
    let rest = &mut rest;
    take(
        rest,
        &["type"],
        |t| (t == "function").then_some(()),
        |_| Some("function".into()),
    )?;
    let name = take(rest, &["name"], string, text)?;
    let parameters = take(
        rest,
        &["parameters"],
        |p| Some(p.clone()),
        |p| Some(p.clone()),
    );
    let mut tool = Tool::new(name, parameters.unwrap_or(Value::Null));
    tool.description = take(rest, &["description"], string, text);
    tool.native = json::native(NAME, std::mem::take(rest));
    Some(tool)
}

fn tools_json(tools: &[Tool]) -> Value {
    let tool = |t: &Tool| {
        let mut v = json!({"type": "function", "name": t.name});
        if let Some(description) = &t.description {
            v["description"] = description.as_str().into();
        }
        if !t.parameters.is_null() {
            v["parameters"] = t.parameters.clone();
            lowercase_types(&mut v["parameters"]);
        }
        merge(&mut v, own(&t.native));
        v
    };
    tools.iter().map(tool).collect()
}

/// `schema` with the types JSON Schema names in lower case so named, as
/// OpenAI takes them: Gemini's schemas name them in upper case, `OBJECT`.
fn lowercase_types(schema: &mut Value) {
    const UPPER: &[&str] = &[
        "STRING", "NUMBER", "INTEGER", "BOOLEAN", "ARRAY", "OBJECT", "NULL",
    ];
    let Some(fields) = schema.as_object_mut() else {
        return;
    };
    for (key, value) in fields.iter_mut() {
        match (key.as_str(), value) {
            ("type", Value::String(t)) if UPPER.contains(&t.as_str()) => {
                *t = t.to_ascii_lowercase();
            }
            ("type", Value::Array(types)) => {
                for t in types {
                    if let Value::String(t) = t
                        && UPPER.contains(&t.as_str())
                    {
                        *t = t.to_ascii_lowercase();
                    }
                }
            }
            ("properties" | "$defs" | "definitions", Value::Object(schemas)) => {
                schemas.values_mut().for_each(lowercase_types);
            }
            ("items" | "additionalProperties" | "not", schema @ Value::Object(_)) => {
                lowercase_types(schema);
            }
            ("anyOf" | "oneOf" | "allOf" | "prefixItems" | "items", Value::Array(schemas)) => {
                schemas.iter_mut().for_each(lowercase_types);
            }
            _ => {}
        }
    }
}

fn tool_choice(v: &Value) -> Option<ToolChoice> {
    match v.as_str() {
        Some("auto") => Some(ToolChoice::Auto),
        Some("none") => Some(ToolChoice::None),
        Some("required") => Some(ToolChoice::Required),
        Some(_) => None,
        None if v["type"] == "function" => Some(ToolChoice::Tool(string(&v["name"])?)),
        None => None,
    }
}

fn tool_choice_json(c: &ToolChoice) -> Option<Value> {
    match c {
        ToolChoice::Auto => Some("auto".into()),
        ToolChoice::None => Some("none".into()),
        ToolChoice::Required => Some("required".into()),
        ToolChoice::Tool(name) => Some(json!({"type": "function", "name": name})),
        _ => None,
    }
}
