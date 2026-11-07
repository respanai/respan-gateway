//! A Live API `setup` as the kind's [`Session`], both ways.

use respan_gateway_core::Fault;
use respan_gateway_realtime::{AudioFormat, Modality, Session, Tool, ToolChoice, Turns, Vad};
use serde_json::{Map, Value, json};

use super::NAME;
use crate::json::{self, at, merge, number, own, set, string, take, text};

const MODEL: &[&str] = &["setup", "model"];
const MODALITIES: &[&str] = &["setup", "generationConfig", "responseModalities"];
const VOICE: &[&str] = &[
    "setup",
    "generationConfig",
    "speechConfig",
    "voiceConfig",
    "prebuiltVoiceConfig",
    "voiceName",
];
const MAX_OUTPUT_TOKENS: &[&str] = &["setup", "generationConfig", "maxOutputTokens"];
const INSTRUCTIONS: &[&str] = &["setup", "systemInstruction", "parts"];
const TOOLS: &[&str] = &["setup", "tools"];
const DETECTION: &[&str] = &["setup", "realtimeInputConfig", "automaticActivityDetection"];
const HANDLING: &[&str] = &["setup", "realtimeInputConfig", "activityHandling"];
const INPUT_TRANSCRIPTION: &[&str] = &["setup", "inputAudioTranscription"];
const OUTPUT_TRANSCRIPTION: &[&str] = &["setup", "outputAudioTranscription"];

/// What `v`, a `setup` message, says; what is left of it is the session's
/// native. The model's audio is always 24 kHz PCM.
pub(super) fn decode(mut v: Value) -> Session {
    let v = &mut v;
    let mut s = Session::default();
    let model = |m: &Value| Some(m.as_str()?.strip_prefix("models/")?.to_owned());
    s.model = take(v, MODEL, model, |m| Some(format!("models/{m}").into()));
    s.output = take(v, MODALITIES, modality, |m| modality_json(*m));
    s.voice = take(v, VOICE, string, text);
    s.max_output_tokens = take(v, MAX_OUTPUT_TOKENS, Value::as_u64, number);
    s.instructions = take(v, INSTRUCTIONS, instructions, |t| {
        Some(json!([{"text": t}]))
    });
    s.tools = take(v, TOOLS, tools, |t| Some(tools_json(t)));
    s.turns = take(v, DETECTION, turns, |t| detection_json(t, refuse).ok());
    if let Some(Turns::Server(vad)) = &mut s.turns {
        vad.interrupt = take(v, HANDLING, handling, |i| handling_json(*i));
    }
    s.input_transcription = transcription(v, INPUT_TRANSCRIPTION);
    s.output_transcription = transcription(v, OUTPUT_TRANSCRIPTION);
    s.output_audio = Some(AudioFormat::pcm16(24_000));
    s.native = json::native(NAME, std::mem::take(v));
    s
}

/// The `setup` message of `s`, refused by `refuse` where it says what the
/// Live API cannot. Its audio formats are each chunk's own.
pub(super) fn encode(s: &Session, refuse: fn(&str) -> Fault) -> Result<Value, Fault> {
    if s.tool_choice
        .as_ref()
        .is_some_and(|c| *c != ToolChoice::Auto)
    {
        return Err(json::refused(
            "Gemini Live has no tool choice, as its model picks tools itself; leave the tool choice `auto` or unset",
        ));
    }
    let mut v = json!({});
    let mut put = |path: &[&str], value: Option<Value>| {
        if let Some(value) = value {
            set(&mut v, path, value);
        }
    };
    put(
        MODEL,
        s.model.as_ref().map(|m| format!("models/{m}").into()),
    );
    put(MODALITIES, s.output.and_then(modality_json));
    put(VOICE, s.voice.as_ref().and_then(text));
    put(
        MAX_OUTPUT_TOKENS,
        s.max_output_tokens.as_ref().and_then(number),
    );
    put(
        INSTRUCTIONS,
        s.instructions.as_ref().map(|t| json!([{"text": t}])),
    );
    put(TOOLS, s.tools.as_ref().map(|t| tools_json(t)));
    put(
        DETECTION,
        s.turns
            .as_ref()
            .map(|t| detection_json(t, refuse))
            .transpose()?,
    );
    if let Some(Turns::Server(vad)) = &s.turns {
        put(HANDLING, vad.interrupt.and_then(handling_json));
    }
    let transcriptions = [
        (INPUT_TRANSCRIPTION, s.input_transcription),
        (OUTPUT_TRANSCRIPTION, s.output_transcription),
    ];
    for (path, on) in transcriptions {
        put(path, on.filter(|on| *on).map(|_| json!({})));
    }
    merge(&mut v, own(&s.native));
    // A transcription turned off is no field, whatever its settings were.
    for (path, on) in transcriptions {
        if on == Some(false) {
            v["setup"].as_object_mut().map(|o| o.shift_remove(path[1]));
        }
    }
    Ok(v)
}

/// A refusal at the client's own edge, never made: the kind's fields of a
/// setup are written back as the Live API's.
fn refuse(_: &str) -> Fault {
    Fault::internal("a setup field could not be written back")
}

/// Whether a transcription is asked for: an object; one that says nothing
/// more is taken, and any settings stay.
fn transcription(v: &mut Value, path: &[&str]) -> Option<bool> {
    let empty = |t: &Value| (*t == json!({})).then_some(true);
    take(v, path, empty, |_| Some(json!({}))).or_else(|| at(v, path)?.is_object().then_some(true))
}

fn modality(v: &Value) -> Option<Modality> {
    match v.as_array()?.as_slice() {
        [m] if m == "AUDIO" => Some(Modality::Audio),
        [m] if m == "TEXT" => Some(Modality::Text),
        _ => None,
    }
}

fn modality_json(m: Modality) -> Option<Value> {
    match m {
        Modality::Audio => Some(json!(["AUDIO"])),
        Modality::Text => Some(json!(["TEXT"])),
        _ => None,
    }
}

/// Instructions of one text part.
fn instructions(parts: &Value) -> Option<String> {
    match parts.as_array()?.as_slice() {
        [part] if part.as_object()?.len() == 1 => string(&part["text"]),
        _ => None,
    }
}

/// The tools of one entry of function declarations.
fn tools(v: &Value) -> Option<Vec<Tool>> {
    match v.as_array()?.as_slice() {
        [] => Some(Vec::new()),
        [entry] if entry.as_object()?.len() == 1 => entry["functionDeclarations"]
            .as_array()?
            .iter()
            .map(tool)
            .collect(),
        _ => None,
    }
}

fn tool(v: &Value) -> Option<Tool> {
    let mut rest = v.clone();
    let rest = &mut rest;
    let name = take(rest, &["name"], string, text)?;
    // JSON Schema, or the OpenAPI subset of it Gemini also takes.
    let schema =
        |key: &str, rest: &mut Value| take(rest, &[key], |p| Some(p.clone()), |p| Some(p.clone()));
    let parameters = schema("parametersJsonSchema", rest).or_else(|| schema("parameters", rest));
    let mut tool = Tool::new(name, parameters.unwrap_or(Value::Null));
    tool.description = take(rest, &["description"], string, text);
    tool.native = json::native(NAME, std::mem::take(rest));
    Some(tool)
}

fn tools_json(tools: &[Tool]) -> Value {
    if tools.is_empty() {
        return json!([]);
    }
    let declaration = |t: &Tool| {
        let mut v = json!({"name": t.name});
        if let Some(description) = &t.description {
            v["description"] = description.as_str().into();
        }
        // Gemini's OpenAPI subset (its types upper case) as it came; any
        // other as JSON Schema, which that subset refuses much of.
        if !t.parameters.is_null() {
            let typed = t.parameters["type"].as_str().unwrap_or_default();
            let openapi = !typed.is_empty() && typed.bytes().all(|b| b.is_ascii_uppercase());
            let key = if openapi {
                "parameters"
            } else {
                "parametersJsonSchema"
            };
            v[key] = t.parameters.clone();
        }
        merge(&mut v, own(&t.native));
        v
    };
    let declarations: Vec<Value> = tools.iter().map(declaration).collect();
    json!([{"functionDeclarations": declarations}])
}

/// Automatic activity detection: off is manual turns, else the server's.
fn turns(v: &Value) -> Option<Turns> {
    if *v == json!({"disabled": true}) {
        return Some(Turns::Manual);
    }
    if v["disabled"] == true {
        return None;
    }
    let mut rest = v.clone();
    let rest = &mut rest;
    let millis = |n: &Value| n.as_u64()?.try_into().ok();
    let millis_json = |n: &u32| Some(Value::from(*n));
    let mut vad = Vad::default();
    vad.silence_ms = take(rest, &["silenceDurationMs"], millis, millis_json);
    vad.prefix_ms = take(rest, &["prefixPaddingMs"], millis, millis_json);
    vad.native = json::native(NAME, std::mem::take(rest));
    Some(Turns::Server(vad))
}

fn detection_json(turns: &Turns, refuse: fn(&str) -> Fault) -> Result<Value, Fault> {
    let vad = match turns {
        Turns::Manual => return Ok(json!({"disabled": true})),
        Turns::Server(vad) => vad,
        _ => return Err(refuse("this turn detection")),
    };
    if vad.respond == Some(false) {
        return Err(json::refused(
            "Gemini Live answers every turn its voice detection ends; leave answering on, or set turn detection to manual",
        ));
    }
    let mut v = Map::new();
    if let Some(ms) = vad.silence_ms {
        v.insert("silenceDurationMs".into(), ms.into());
    }
    if let Some(ms) = vad.prefix_ms {
        v.insert("prefixPaddingMs".into(), ms.into());
    }
    let mut v = Value::Object(v);
    merge(&mut v, own(&vad.native));
    Ok(v)
}

/// Whether the user's activity interrupts the model.
fn handling(v: &Value) -> Option<bool> {
    match v.as_str()? {
        "START_OF_ACTIVITY_INTERRUPTS" => Some(true),
        "NO_INTERRUPTION" => Some(false),
        _ => None,
    }
}

fn handling_json(interrupts: bool) -> Option<Value> {
    Some(match interrupts {
        true => "START_OF_ACTIVITY_INTERRUPTS".into(),
        false => "NO_INTERRUPTION".into(),
    })
}
