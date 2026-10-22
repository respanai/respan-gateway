//! What the model says: the Live API's server messages as the kind's
//! [`Output`], read at the provider's edge and written at the client's.
//!
//! A message may say several things, each an event. What a message says
//! beyond its events stays on the first event its client writes a message
//! of, so a message is written back as it came; a message whose events
//! would be written as several, such as a turn of two parts, reaches its
//! client as several, each saying its part.

use std::sync::atomic::Ordering;

use respan_gateway_core::{Fault, NativeFields, NativeValue};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_realtime::{Audio, AudioFormat, Cause, Output, Purpose, Status, Usage};
use respan_gateway_ws::CloseCode;
use serde_json::{Map, Value, json};

use super::{Answering, NAME, Shared, State};
use crate::audio::Convert;
use crate::foreign;
use crate::json::{self, at, count, merge, own, set, string, take, text, untranslatable_output};

/// What a turn the client cancelled says that is dropped: its content, and
/// its end, as its response already ended.
const CANCELLED: &[&[&str]] = &[
    &["serverContent", "modelTurn"],
    &["serverContent", "outputTranscription"],
    &["serverContent", "turnComplete"],
    &["serverContent", "turnCompleteReason"],
    &["toolCall"],
];

/// A provider's messages as events.
#[derive(Debug)]
pub(super) struct Provider {
    /// What it shares with the writer of the client's events.
    shared: Shared,
    /// Whether the turn's response ended, by its calls or an interruption,
    /// before the turn did.
    ended: bool,
    /// Whether `voiceActivity` said the user began speaking, since the
    /// last interruption.
    spoke: bool,
    /// Whether an interruption said the user began speaking before
    /// `voiceActivity`, which the Live API sends just after it.
    told: bool,
    /// What was heard of the user's turn, in a transcription.
    heard: String,
    /// Whether the model answers the user's turn, in a transcription,
    /// whose answer is dropped.
    answered: bool,
}

impl Provider {
    pub(super) fn new(shared: Shared) -> Self {
        Self {
            shared,
            ended: false,
            spoke: false,
            told: false,
            heard: String::new(),
            answered: false,
        }
    }

    pub(super) fn read(&mut self, raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
        let mut v: Value = serde_json::from_slice(&raw)
            .map_err(|_| Fault::upstream("a provider's message is not JSON"))?;
        if !v.is_object() {
            out.push(Output::native(NAME, v));
            return Ok(());
        }
        // A model that only transcribes ends its generation, never a turn.
        let generated = v["serverContent"]["generationComplete"] == true;
        let used = usage(&mut v);
        let mut events = Vec::new();
        let shared = self.shared.clone();
        let mut said = Vec::new();
        if self.decode(&mut shared.lock(), &mut v, &mut events) {
            said.push(Output::ResponseStart {
                id: None,
                native: None,
            });
        }
        said.extend(used.map(Output::Usage));
        let rest = v.as_object().filter(|rest| !rest.is_empty());
        match events
            .iter_mut()
            .find(|e| !matches!(e, Output::ToolCall { .. }))
        {
            Some(first) => keep(first, rest),
            None => events.extend(rest.map(|rest| Output::native(NAME, rest.clone()))),
        }
        said.extend(events);
        let purpose = shared.lock().applied.as_ref().and_then(|s| s.purpose);
        if purpose != Some(Purpose::Transcription) {
            out.extend(said);
            return Ok(());
        }
        for event in said {
            self.transcribed(event, out);
        }
        if generated && !self.answered {
            out.extend(self.whole());
        }
        Ok(())
    }

    /// `event` in a transcription, which the Live API answers all the
    /// same: its answer is dropped, and the user's transcript is whole when
    /// the answer is (all the user said of its turn has come by then), or
    /// when the user's next turn begins.
    fn transcribed(&mut self, event: Output, out: &mut Vec<Output>) {
        match event {
            Output::InputTranscript {
                ref text,
                complete: false,
                ..
            } => {
                self.heard.push_str(text);
                out.push(event);
            }
            Output::SpeechStarted { .. } => {
                out.extend(self.whole());
                out.push(event);
            }
            Output::ResponseStart { .. } => self.answered = true,
            Output::ResponseDone { status, .. } => {
                self.answered = false;
                if status == Status::Completed {
                    out.extend(self.whole());
                }
            }
            Output::Audio { .. }
            | Output::Text { .. }
            | Output::Transcript { .. }
            | Output::ToolCall { .. } => {}
            event => out.push(event),
        }
    }

    /// The user's whole transcript of its turn, if anything was heard.
    fn whole(&mut self) -> Option<Output> {
        (!self.heard.is_empty()).then(|| Output::InputTranscript {
            text: std::mem::take(&mut self.heard),
            complete: true,
            native: None,
        })
    }

    /// The events of what `v` says, taken from it; whether a response
    /// begins before them.
    fn decode(&mut self, state: &mut State, v: &mut Value, events: &mut Vec<Output>) -> bool {
        let mut starts = false;
        let mut start = |open: &mut bool, ended: &mut bool| {
            starts |= !*open;
            (*open, *ended) = (true, false);
        };
        if state.cancelled {
            let content = &v["serverContent"];
            // An interrupted turn still ends, which says nothing more.
            self.ended |= content["interrupted"] == true;
            state.cancelled = content["turnComplete"] != true && content["interrupted"] != true;
            for path in CANCELLED {
                json::remove(v, path);
            }
        }
        if let Some(ids) = at(v, &["toolCallCancellation", "ids"]).and_then(Value::as_array) {
            let ids = ids.iter().filter_map(Value::as_str).map(str::to_owned);
            state.withdrawn.extend(ids);
        }
        if let Some(Value::Object(complete)) = v.get("setupComplete").cloned() {
            json::remove(v, &["setupComplete"]);
            let mut session = state.applied.clone().unwrap_or_default();
            session.native = match complete.is_empty() {
                true => None,
                false => json::native(NAME, json!({"setupComplete": complete})),
            };
            events.push(Output::Updated(session));
        }
        let activity = at(v, &["voiceActivity", "type"]).and_then(Value::as_str);
        let activity = activity.map(str::to_owned);
        let activity_event = |v: &mut Value| {
            let at_ms = take(v, &["voiceActivity", "audioOffset"], millis, offset);
            let native = json::native(NAME, json!({"voiceActivity": v["voiceActivity"]}));
            json::remove(v, &["voiceActivity"]);
            (at_ms, native)
        };
        match activity.as_deref() {
            // The interruption already said so.
            Some("ACTIVITY_START") if std::mem::take(&mut self.told) => {}
            Some("ACTIVITY_START") => {
                self.spoke = true;
                let (at_ms, native) = activity_event(v);
                events.push(Output::SpeechStarted { at_ms, native });
            }
            Some("ACTIVITY_END") => {
                let (at_ms, native) = activity_event(v);
                events.push(Output::SpeechStopped { at_ms, native });
            }
            _ => {}
        }
        if at(v, &["serverContent", "interrupted"]) == Some(&Value::Bool(true)) {
            // An interruption of no response stays the Live API's to say.
            let said = state.open;
            if !self.spoke {
                self.told = true;
                events.push(Output::SpeechStarted {
                    at_ms: None,
                    native: None,
                });
            }
            if state.open {
                events.push(Output::ResponseDone {
                    status: Status::Interrupted,
                    native: None,
                });
                (state.open, self.ended) = (false, true);
            }
            self.spoke = false;
            if said {
                json::remove(v, &["serverContent", "interrupted"]);
            }
        }
        let path = ["serverContent", "inputTranscription", "text"];
        if let Some(heard) = take(v, &path, string, text) {
            events.push(Output::InputTranscript {
                text: heard,
                complete: false,
                native: None,
            });
        }
        let path = ["serverContent", "modelTurn", "parts"];
        let parts = at(v, &path).and_then(Value::as_array);
        if let Some(parts) = parts.and_then(|p| p.iter().map(part).collect::<Option<Vec<_>>>()) {
            self.told = false;
            start(&mut state.open, &mut self.ended);
            events.extend(parts);
            json::remove(v, &path);
        }
        let path = ["serverContent", "outputTranscription", "text"];
        if let Some(said) = take(v, &path, string, text) {
            start(&mut state.open, &mut self.ended);
            events.push(Output::Transcript {
                text: said,
                native: None,
            });
        }
        let path = ["toolCall", "functionCalls"];
        let calls = at(v, &path).and_then(Value::as_array);
        if let Some(calls) = calls.and_then(|c| c.iter().map(call).collect::<Option<Vec<_>>>()) {
            start(&mut state.open, &mut self.ended);
            for call in &calls {
                if let Output::ToolCall { id, name, .. } = call {
                    state.calls.insert(id.clone(), name.clone());
                }
            }
            events.extend(calls);
            events.push(Output::ResponseDone {
                status: Status::Completed,
                native: None,
            });
            json::remove(v, &path);
            (state.open, self.ended) = (false, true);
        }
        // A turn whose response already ended says nothing more: its end
        // stays the Live API's.
        if at(v, &["serverContent", "turnComplete"]) == Some(&Value::Bool(true)) && !self.ended {
            start(&mut state.open, &mut self.ended);
            json::remove(v, &["serverContent", "turnComplete"]);
            let reason = take(v, &["serverContent", "turnCompleteReason"], string, text);
            events.push(Output::ResponseDone {
                status: reason.map_or(Status::Completed, status),
                native: None,
            });
            state.open = false;
        } else if at(v, &["serverContent", "turnComplete"]).is_some() {
            self.ended = false;
        }
        starts
    }
}

/// Keep `rest`, what a message says beyond its events, on `event`.
fn keep(event: &mut Output, rest: Option<&Map<String, Value>>) {
    let Some(rest) = rest else {
        return;
    };
    let native = match event {
        Output::Opened(s) | Output::Updated(s) => &mut s.native,
        Output::SpeechStarted { native, .. }
        | Output::SpeechStopped { native, .. }
        | Output::InputTranscript { native, .. }
        | Output::ResponseStart { native, .. }
        | Output::Audio { native, .. }
        | Output::Text { native, .. }
        | Output::Transcript { native, .. }
        | Output::ToolCall { native, .. }
        | Output::ResponseDone { native, .. }
        | Output::Error { native, .. } => native,
        _ => return,
    };
    let fields = &mut native
        .get_or_insert_with(|| NativeFields::new(NAME, Map::new()))
        .fields;
    let mut all = Value::Object(std::mem::take(fields));
    merge(&mut all, Some(rest));
    if let Value::Object(all) = all {
        *fields = all;
    }
}

/// A part of the model's turn: its text, or its audio as PCM at the rate
/// its type declares; `None` for any other.
fn part(p: &Value) -> Option<Output> {
    let (field, value) = match p.as_object()? {
        fields if fields.len() == 1 => fields.iter().next()?,
        _ => return None,
    };
    match field.as_str() {
        "text" => Some(Output::Text {
            text: value.as_str()?.to_owned(),
            native: None,
        }),
        "inlineData" if value.as_object()?.len() == 2 => {
            let rate = value["mimeType"]
                .as_str()?
                .strip_prefix("audio/pcm;rate=")?;
            Some(Output::Audio {
                audio: Audio {
                    data: json::unbase64(&value["data"])?,
                    format: AudioFormat::pcm16(rate.parse().ok()?),
                },
                native: None,
            })
        }
        _ => None,
    }
}

/// A function call: its arguments as JSON text, its other fields kept.
fn call(c: &Value) -> Option<Output> {
    let mut rest = c.as_object()?.clone();
    let id = rest.shift_remove("id")?.as_str()?.to_owned();
    let name = rest.shift_remove("name")?.as_str()?.to_owned();
    let arguments = match rest.shift_remove("args") {
        Some(args @ Value::Object(_)) => args.to_string(),
        None => "{}".into(),
        Some(_) => return None,
    };
    Some(Output::ToolCall {
        id,
        name,
        arguments,
        native: json::native(NAME, Value::Object(rest)),
    })
}

/// Milliseconds of a duration as the Live API writes one, such as
/// `"0.760s"` or `"8s"`.
fn millis(v: &Value) -> Option<u64> {
    let seconds = v.as_str()?.strip_suffix('s')?;
    let (whole, fraction) = seconds.split_once('.').unwrap_or((seconds, ""));
    let fraction = format!("{fraction:0<3}");
    Some(whole.parse::<u64>().ok()? * 1_000 + fraction.get(..3)?.parse::<u64>().ok()?)
}

/// A duration of `ms` as the Live API writes one.
fn offset(ms: &u64) -> Option<Value> {
    Some(
        match ms % 1_000 {
            0 => format!("{}s", ms / 1_000),
            rest => format!("{}.{rest:03}s", ms / 1_000),
        }
        .into(),
    )
}

/// How a turn ended, by the reason the Live API gives.
fn status(reason: String) -> Status {
    match reason.as_str() {
        "MALFORMED_FUNCTION_CALL" => Status::Failed,
        _ => Status::Native {
            format: NAME.into(),
            reason,
            fallback: Box::new(Status::Filtered),
        },
    }
}

/// The reason the Live API gives for `status`, if any.
fn reason(status: &Status) -> Option<&str> {
    match status {
        Status::Failed => Some("MALFORMED_FUNCTION_CALL"),
        Status::Native { format, reason, .. } if **format == *NAME => Some(reason),
        Status::Native { fallback, .. } => self::reason(fallback),
        _ => None,
    }
}

/// The usage `v` reports, its counts above zero taken; the rest of its
/// `usageMetadata` stays, if empty, as the message's.
fn usage(v: &mut Value) -> Option<Usage> {
    let metadata = v.get_mut("usageMetadata")?.as_object_mut()?;
    let mut taken = |key: &str| match metadata.get(key).and_then(count) {
        Some(n) => {
            metadata.shift_remove(key);
            n
        }
        None => 0,
    };
    let (prompt, response) = (taken("promptTokenCount"), taken("responseTokenCount"));
    let thoughts = taken("thoughtsTokenCount");
    let mut used = Usage::default();
    used.tokens.cached_input_tokens = taken("cachedContentTokenCount");
    let tool_use = metadata
        .get("toolUsePromptTokenCount")
        .and_then(Value::as_u64);
    used.tokens.input_tokens = prompt + tool_use.unwrap_or(0);
    used.tokens.output_tokens = response + thoughts;
    used.tokens.reasoning_tokens = thoughts;
    used.input_audio_tokens = audio(metadata.get_mut("promptTokensDetails"));
    used.output_audio_tokens = audio(metadata.get_mut("responseTokensDetails"));
    Some(used)
}

/// The count of the `AUDIO` entry of `details`, taken.
fn audio(details: Option<&mut Value>) -> u64 {
    let entries = details.and_then(Value::as_array_mut).into_iter().flatten();
    for entry in entries.filter(|e| e["modality"] == "AUDIO") {
        if let (Some(n), Some(fields)) = (
            entry.get("tokenCount").and_then(count),
            entry.as_object_mut(),
        ) {
            fields.shift_remove("tokenCount");
            return n;
        }
    }
    0
}

/// Write `used` in `v`'s `usageMetadata`: each count above zero, over what
/// the message kept.
fn put_usage(v: &mut Value, used: &Usage) {
    let at = |key| ["usageMetadata", key];
    let tool_use = v["usageMetadata"]["toolUsePromptTokenCount"]
        .as_u64()
        .unwrap_or(0);
    let tokens = &used.tokens;
    let counts = [
        (
            "promptTokenCount",
            tokens.input_tokens.saturating_sub(tool_use),
        ),
        (
            "responseTokenCount",
            tokens.output_tokens.saturating_sub(tokens.reasoning_tokens),
        ),
        ("thoughtsTokenCount", tokens.reasoning_tokens),
        ("cachedContentTokenCount", tokens.cached_input_tokens),
    ];
    for (key, n) in counts.into_iter().filter(|(_, n)| *n > 0) {
        set(v, &at(key), n.into());
    }
    let details = [
        ("promptTokensDetails", used.input_audio_tokens),
        ("responseTokensDetails", used.output_audio_tokens),
    ];
    for (key, n) in details.into_iter().filter(|(_, n)| *n > 0) {
        let entries = &mut v["usageMetadata"][key];
        if !entries.is_array() {
            *entries = json!([]);
        }
        let Some(entries) = entries.as_array_mut() else {
            continue;
        };
        match entries.iter_mut().find(|e| e["modality"] == "AUDIO") {
            Some(entry) => entry["tokenCount"] = n.into(),
            None => entries.push(json!({"modality": "AUDIO", "tokenCount": n})),
        }
    }
}

/// The events written to the client. Another format's natives are
/// dropped, but its MCP events, which only a session of MCP tools has,
/// refused; a whole transcript of the user's turn is written only if no
/// part of it was.
#[derive(Debug)]
pub(super) struct Client {
    /// Whether its setup's completion was written.
    setup: bool,
    /// Usage held for the next message.
    usage: Option<Usage>,
    /// The response's calls, written together at its end.
    calls: Vec<Value>,
    /// Whether the response's interruption was written.
    interrupted: bool,
    /// Whether the response began in another format, whose end the Live
    /// API's own turn's end does not follow.
    foreign: bool,
    /// Whether part of the user's turn's transcript was written.
    heard: bool,
    /// Whether a response is being written, which its reader reads.
    answering: Answering,
    /// The model's audio as 24 kHz PCM.
    convert: Convert,
}

impl Client {
    pub(super) fn new(answering: Answering) -> Self {
        Self {
            setup: false,
            usage: None,
            calls: Vec::new(),
            interrupted: false,
            foreign: false,
            heard: false,
            answering,
            convert: Convert::default(),
        }
    }

    pub(super) fn write(&mut self, event: &Output, out: &mut Vec<Bytes>) -> Result<(), Fault> {
        if let Output::ResponseStart { native, .. } = event {
            self.foreign = native.as_ref().is_some_and(|n| *n.format != *NAME);
        }
        let Some(event) = foreign::strip(NAME, event)? else {
            return Ok(());
        };
        match &event {
            // A response's audio starts afresh: none the last one's
            // resampler held spills into it.
            Output::ResponseStart { .. } => {
                self.answering.store(true, Ordering::Relaxed);
                self.convert = Convert::default();
            }
            Output::ResponseDone { .. } => self.answering.store(false, Ordering::Relaxed),
            _ => {}
        }
        for v in self.encode(&event)? {
            out.push(json::frame(&v)?);
        }
        Ok(())
    }

    fn encode(&mut self, event: &Output) -> Result<Vec<Value>, Fault> {
        let refuse = |what: &str| untranslatable_output(NAME, what);
        let interrupted = json!({"serverContent": {"interrupted": true}});
        let content = |field: &str, value: Value| json!({"serverContent": {field: value}});
        let (v, native) = match event {
            Output::Opened(_) => return Ok(Vec::new()),
            Output::Updated(s) if !std::mem::replace(&mut self.setup, true) => {
                (json!({"setupComplete": {}}), &s.native)
            }
            Output::Updated(_) => return Ok(Vec::new()),
            Output::SpeechStarted { at_ms, native } if says(native, "voiceActivity") => {
                (activity("ACTIVITY_START", at_ms), native)
            }
            // Speech interrupts a response being written; else it is
            // written only as the Live API said it, if it did.
            Output::SpeechStarted { native, .. } if self.answering.load(Ordering::Relaxed) => {
                self.interrupted = true;
                (interrupted, native)
            }
            Output::SpeechStarted { native: None, .. } => return Ok(Vec::new()),
            Output::SpeechStarted { native, .. } => (json!({}), native),
            Output::SpeechStopped { at_ms, native } => (activity("ACTIVITY_END", at_ms), native),
            Output::InputTranscript { complete: true, .. } if std::mem::take(&mut self.heard) => {
                return Ok(Vec::new());
            }
            Output::InputTranscript {
                text,
                complete,
                native,
            } => {
                self.heard = !complete;
                (content("inputTranscription", json!({"text": text})), native)
            }
            Output::ResponseStart { native, .. } => {
                own(NAME, native, || refuse("another format's event"))?;
                self.interrupted = false;
                return Ok(Vec::new());
            }
            Output::Audio { audio, native } => {
                // The Live API's audio is always 24 kHz PCM.
                let pcm = AudioFormat::pcm16(24_000);
                let converted = self.convert.to(audio, pcm);
                let converted = converted.ok_or_else(|| refuse("audio of this format"))?;
                let mime = format!("audio/pcm;rate={}", pcm.rate);
                let data = json!({"mimeType": mime, "data": json::base64(&converted)});
                (
                    content("modelTurn", json!({"parts": [{"inlineData": data}]})),
                    native,
                )
            }
            Output::Text { text, native } => (
                content("modelTurn", json!({"parts": [{"text": text}]})),
                native,
            ),
            Output::Transcript { text, native } => (
                content("outputTranscription", json!({"text": text})),
                native,
            ),
            Output::ToolCall {
                id,
                name,
                arguments,
                native,
            } => {
                let args: Value = serde_json::from_str(arguments)
                    .ok()
                    .filter(Value::is_object)
                    .ok_or_else(|| {
                        Fault::upstream("a tool call's arguments are not a JSON object")
                    })?;
                let mut call = json!({"id": id, "name": name, "args": args});
                merge(
                    &mut call,
                    own(NAME, native, || refuse("another format's call"))?,
                );
                self.calls.push(call);
                return Ok(Vec::new());
            }
            Output::Usage(used) => {
                *self.usage.get_or_insert_default() += used;
                return Ok(Vec::new());
            }
            // The Live API ends a turn of calls, or one interrupted, with
            // its turn's end; a response of its own is followed by it.
            Output::ResponseDone { status, native } => {
                let interrupted_already = std::mem::take(&mut self.interrupted);
                let turn_complete = content("turnComplete", true.into());
                let (v, then) = match (std::mem::take(&mut self.calls), status) {
                    (calls, _) if !calls.is_empty() => {
                        (json!({"toolCall": {"functionCalls": calls}}), true)
                    }
                    (_, Status::Interrupted) if interrupted_already && self.foreign => {
                        (turn_complete, false)
                    }
                    (_, Status::Interrupted) if interrupted_already => {
                        own(NAME, native, || refuse("another format's event"))?;
                        return Ok(Vec::new());
                    }
                    (_, Status::Interrupted) => (interrupted, true),
                    (_, status) => {
                        let mut v = turn_complete;
                        if let Some(reason) = reason(status) {
                            v["serverContent"]["turnCompleteReason"] = reason.into();
                        }
                        (v, false)
                    }
                };
                let mut v = v;
                merge(
                    &mut v,
                    own(NAME, native, || refuse("another format's event"))?,
                );
                let mut messages = vec![v];
                if then && self.foreign {
                    messages.push(content("turnComplete", true.into()));
                }
                if let (Some(used), Some(last)) = (self.usage.take(), messages.last_mut()) {
                    put_usage(last, &used);
                }
                return Ok(messages);
            }
            Output::Error { cause, message, .. } => {
                // A Live API client knows a refusal only as its session's
                // close; a cancel that found nothing changed nothing.
                return match cause {
                    Cause::NothingToCancel => Ok(Vec::new()),
                    Cause::Invalid => {
                        Err(Fault::bad_request(message.clone()).with(CloseCode(1007)))
                    }
                    _ => Err(Fault::upstream(message.clone())),
                };
            }
            Output::Native(NativeValue { format, value }) if **format == *NAME => {
                (Value::clone(value), &None)
            }
            _ => return Err(refuse("another format's event")),
        };
        let mut v = v;
        merge(
            &mut v,
            own(NAME, native, || refuse("another format's event"))?,
        );
        if let Some(used) = self.usage.take() {
            put_usage(&mut v, &used);
        }
        Ok(vec![v])
    }
}

/// A `voiceActivity` message of `kind`, at `at_ms` into the user's audio.
fn activity(kind: &str, at_ms: &Option<u64>) -> Value {
    let mut v = json!({"voiceActivity": {"type": kind}});
    if let Some(ms) = at_ms.as_ref().and_then(offset) {
        v["voiceActivity"]["audioOffset"] = ms;
    }
    v
}

/// Whether `native` is this format's and holds `key`.
fn says(native: &Option<NativeFields>, key: &str) -> bool {
    native
        .as_ref()
        .is_some_and(|n| *n.format == *NAME && n.fields.contains_key(key))
}
