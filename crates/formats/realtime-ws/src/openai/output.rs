//! What the model says: OpenAI Realtime's server events as the kind's
//! [`Output`], read at the provider's edge and written at the client's.

use respan_gateway_http::native::Json;
use std::collections::HashMap;

use respan_gateway_core::{Fault, Native};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_realtime::{Audio, AudioFormat, Cause, Output, Status, Usage};
use serde_json::{Map, Value, json};

use super::{Key, NAME, session};
use crate::audio::Convert;
use crate::foreign;
use crate::json::{self, at, count, merge, number, set, string, take, text, untranslatable_output};

/// A count of [`Usage`].
type Counter = fn(&mut Usage) -> &mut u64;

/// Where a response's usage counts are, and which of [`Usage`] each is.
const USAGE: &[(&[&str], Counter)] = &[
    (&["response", "usage", "input_tokens"], |u| {
        &mut u.tokens.input_tokens
    }),
    (&["response", "usage", "output_tokens"], |u| {
        &mut u.tokens.output_tokens
    }),
    (
        &["response", "usage", "input_token_details", "cached_tokens"],
        |u| &mut u.tokens.cached_input_tokens,
    ),
    (
        &["response", "usage", "input_token_details", "audio_tokens"],
        |u| &mut u.input_audio_tokens,
    ),
    (
        &["response", "usage", "output_token_details", "audio_tokens"],
        |u| &mut u.output_audio_tokens,
    ),
];

/// A provider's frames as events.
#[derive(Debug)]
pub(super) struct Provider {
    /// The format of the model's audio, as the session says.
    output: AudioFormat,
    /// The tools' names by the id of their calls, for a call's end that
    /// does not name its tool.
    calls: HashMap<String, String>,
}

impl Default for Provider {
    fn default() -> Self {
        Self {
            output: AudioFormat::pcm16(24_000),
            calls: HashMap::new(),
        }
    }
}

impl Provider {
    pub(super) fn read(&mut self, raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
        let v: Value = serde_json::from_slice(&raw)
            .map_err(|_| Fault::upstream("a provider's frame is not a JSON event"))?;
        let mut rest = v.clone();
        match take(&mut rest, &["type"], string, text) {
            Some(kind) if self.decode(&kind, rest, out).is_some() => {}
            _ => out.push(Output::Native(Native::json::<Key>(v))),
        }
        Ok(())
    }

    /// The events of a server event of type `kind`, whose other fields are
    /// `rest`; `None`, writing none, for one the kind has no form for or
    /// one it could not write back as it came.
    fn decode(&mut self, kind: &str, mut rest: Value, out: &mut Vec<Output>) -> Option<()> {
        let v = &mut rest;
        let delta = |v: &mut Value| take(v, &["delta"], string, text);
        let event = match kind {
            "session.created" | "session.updated" => {
                let session = session::decode(rest);
                self.output = session.output_audio.unwrap_or(self.output);
                match kind {
                    "session.created" => Output::Opened(session),
                    _ => Output::Updated(session),
                }
            }
            "input_audio_buffer.speech_started" => Output::SpeechStarted {
                at_ms: take(v, &["audio_start_ms"], |n| n.as_u64(), number),
                native: Native::json::<Key>(rest),
            },
            "input_audio_buffer.speech_stopped" => Output::SpeechStopped {
                at_ms: take(v, &["audio_end_ms"], |n| n.as_u64(), number),
                native: Native::json::<Key>(rest),
            },
            "conversation.item.input_audio_transcription.delta" => Output::InputTranscript {
                text: delta(v)?,
                complete: false,
                native: Native::json::<Key>(rest),
            },
            "conversation.item.input_audio_transcription.completed" => Output::InputTranscript {
                text: take(v, &["transcript"], string, text)?,
                complete: true,
                native: Native::json::<Key>(rest),
            },
            // A response's start is OpenAI's even if it says nothing more:
            // its items are the provider's.
            "response.created" => {
                let native = match rest.as_object()? {
                    rest if rest.is_empty() => Native::own::<Key>(),
                    rest => Native::json::<Key>(rest.clone()),
                };
                Output::ResponseStart { native }
            }
            "response.output_audio.delta" => Output::Audio {
                audio: Audio {
                    data: json::take_audio(v, &["delta"])?,
                    format: self.output,
                },
                native: Native::json::<Key>(rest),
            },
            "response.output_text.delta" => Output::Text {
                text: delta(v)?,
                native: Native::json::<Key>(rest),
            },
            "response.output_audio_transcript.delta" => Output::Transcript {
                text: delta(v)?,
                native: Native::json::<Key>(rest),
            },
            "response.output_item.added" => {
                let item = &v["item"];
                if let (Some("function_call"), Some(id), Some(name)) = (
                    item["type"].as_str(),
                    item["call_id"].as_str(),
                    item["name"].as_str(),
                ) {
                    self.calls.insert(id.to_owned(), name.to_owned());
                }
                return None;
            }
            "response.function_call_arguments.done" => {
                let id = take(v, &["call_id"], string, text)?;
                let name =
                    take(v, &["name"], string, text).or_else(|| self.calls.get(&id).cloned())?;
                Output::ToolCall {
                    arguments: take(v, &["arguments"], string, text)?,
                    id,
                    name,
                    native: Native::json::<Key>(rest),
                }
            }
            "response.done" => {
                let status = status(v)?;
                if at(v, &["response", "usage"]).is_some_and(Value::is_object) {
                    let mut usage = Usage::default();
                    for (path, counter) in USAGE {
                        *counter(&mut usage) = take(v, path, count, number).unwrap_or(0);
                    }
                    out.push(Output::Usage(usage));
                }
                Output::ResponseDone {
                    status,
                    native: Native::json::<Key>(rest),
                }
            }
            "error" => {
                let cause = match (v["error"]["type"].as_str(), v["error"]["code"].as_str()) {
                    (_, Some("response_cancel_not_active")) => Cause::NothingToCancel,
                    (Some("invalid_request_error"), _) => Cause::Invalid,
                    _ => Cause::Provider,
                };
                Output::Error {
                    cause,
                    message: take(v, &["error", "message"], string, text)?,
                    native: Native::json::<Key>(rest),
                }
            }
            _ => return None,
        };
        out.push(event);
        Some(())
    }
}

/// How a response ended, by its status and the reason its details give,
/// taken from `v` where they write back as they came.
fn status(v: &mut Value) -> Option<Status> {
    let said = string(at(v, &["response", "status"])?)?;
    let details = at(v, &["response", "status_details"]);
    let typed = details
        .and_then(|d| d.get("type"))
        .is_none_or(|t| *t == *said);
    let reason = details.and_then(|d| d["reason"].as_str());
    let known = [Status::Interrupted, Status::Cancelled, Status::Filtered];
    let named = known
        .into_iter()
        .find(|s| typed && spelled(s) == Some((said.as_str(), reason)));
    let status = match (said.as_str(), named) {
        (_, Some(status)) => {
            take(v, &["response", "status_details", "type"], string, text);
            take(v, &["response", "status_details", "reason"], string, text);
            status
        }
        ("completed", None) => Status::Completed,
        ("failed", None) => Status::Failed,
        (_, None) => Status::Native {
            native: Native::json::<Key>(said.as_str()),
            // A response cut at its token limit is whole as far as it went.
            fallback: Box::new(match (said.as_str(), reason) {
                ("cancelled", _) => Status::Cancelled,
                ("incomplete", Some("max_output_tokens")) => Status::Completed,
                _ => Status::Failed,
            }),
        },
    };
    take(v, &["response", "status"], string, text);
    Some(status)
}

/// A status as GA writes it: the response's status, and its details'
/// reason if it has one.
fn spelled(status: &Status) -> Option<(&str, Option<&str>)> {
    Some(match status {
        Status::Completed => ("completed", None),
        Status::Interrupted => ("cancelled", Some("turn_detected")),
        Status::Cancelled => ("cancelled", Some("client_cancelled")),
        Status::Filtered => ("incomplete", Some("content_filter")),
        Status::Failed => ("failed", None),
        Status::Native { native, fallback } => match native.value::<Key>() {
            Some(Value::String(reason)) => (reason.as_str(), None),
            _ => return spelled(fallback),
        },
        _ => return None,
    })
}

/// The events written to the client. A response that began in another
/// format has none of OpenAI's items, so they are made up for it, with
/// ids of their own, and told of as OpenAI does: the response, its message
/// and content part, their deltas and ends, an item per call, and the
/// response's end with its usage and items.
#[derive(Debug)]
pub(super) struct Client {
    /// The format of the audio it takes, as its session says.
    output: AudioFormat,
    /// The model's audio in that format.
    convert: Convert,
    /// Usage of the response being made, written in its `response.done`.
    usage: Option<Usage>,
    ids: Ids,
    /// The response being made, if it began in another format.
    response: Option<Response>,
    /// The item of the user's turn another format's events speak of.
    user: Option<String>,
    /// What was heard of it, until its whole transcript is written.
    heard: Option<String>,
}

impl Default for Client {
    fn default() -> Self {
        Self {
            output: AudioFormat::pcm16(24_000),
            convert: Convert::default(),
            usage: None,
            ids: Ids::default(),
            response: None,
            user: None,
            heard: None,
        }
    }
}

impl Client {
    pub(super) fn write(&mut self, event: &Output, out: &mut Vec<Bytes>) -> Result<(), Fault> {
        let Some(event) = foreign::strip::<Key>(event) else {
            return Ok(());
        };
        // A response's audio starts afresh: none the last one's resampler
        // held spills into it.
        if let Output::ResponseStart { .. } = event {
            self.convert = Convert::default();
        }
        let mut events = Vec::new();
        self.encode(&event, &mut events)?;
        let made_up = !owned(&event);
        for mut v in events {
            if made_up && v.get("event_id").is_none() {
                v["event_id"] = self.ids.next("event").into();
            }
            out.push(json::frame(&v)?);
        }
        Ok(())
    }

    fn encode(&mut self, event: &Output, out: &mut Vec<Value>) -> Result<(), Fault> {
        let refuse = |what: &str| untranslatable_output(NAME, what);
        let (mut v, native) = match event {
            Output::Opened(s) | Output::Updated(s) => {
                self.output = s.output_audio.unwrap_or(self.output);
                let kind = match event {
                    Output::Opened(_) => "session.created",
                    _ => "session.updated",
                };
                let mut session = session::encode(s, refuse)?;
                if s.native.fields::<Key>().is_empty() && session.get("type").is_none() {
                    session["type"] = "realtime".into();
                }
                (json!({"type": kind, "session": session}), &s.native)
            }
            Output::SpeechStarted { at_ms, native } => {
                let mut v = json!({"type": "input_audio_buffer.speech_started"});
                if let Some(ms) = at_ms {
                    v["audio_start_ms"] = (*ms).into();
                }
                if native.fields::<Key>().is_empty() {
                    let item = self.ids.next("item");
                    v["item_id"] = item.as_str().into();
                    self.user = Some(item);
                }
                (v, native)
            }
            Output::SpeechStopped { at_ms, native } => {
                let mut v = json!({"type": "input_audio_buffer.speech_stopped"});
                if let Some(ms) = at_ms {
                    v["audio_end_ms"] = (*ms).into();
                }
                if native.fields::<Key>().is_empty() {
                    v["item_id"] = self.user().into();
                }
                (v, native)
            }
            Output::InputTranscript {
                text,
                complete,
                native,
            } => {
                let kind = "conversation.item.input_audio_transcription";
                let mut v = match complete {
                    false => json!({"type": format!("{kind}.delta"), "delta": text}),
                    true => json!({"type": format!("{kind}.completed"), "transcript": text}),
                };
                if native.fields::<Key>().is_empty() {
                    v["item_id"] = self.user().into();
                    v["content_index"] = 0.into();
                    match complete {
                        false => self.heard.get_or_insert_default().push_str(text),
                        // Its item is whole: the next transcript is another's.
                        true => (self.heard, self.user) = (None, None),
                    }
                }
                (v, native)
            }
            Output::ResponseStart { native } if native.value::<Key>().is_none() => {
                // What was heard of the turn it answers is all there is.
                if let Some(heard) = self.heard.take() {
                    out.push(
                        json!({"type": "conversation.item.input_audio_transcription.completed",
                        "item_id": self.user(), "content_index": 0, "transcript": heard}),
                    );
                }
                self.user = None;
                let id = self.ids.next("resp");
                out.push(json!({"type": "response.created", "response": {
                    "object": "realtime.response", "id": id, "status": "in_progress",
                    "status_details": null, "output": [], "usage": null}}));
                self.response = Some(Response {
                    id,
                    said: None,
                    output: Vec::new(),
                });
                return Ok(());
            }
            Output::ResponseStart { native } => {
                self.response = None;
                (json!({"type": "response.created", "response": {}}), native)
            }
            Output::Audio { audio, native } => {
                let data = self.convert.to(audio, self.output);
                let data = data.ok_or_else(|| refuse("audio of this format"))?;
                let delta = json::base64(&data);
                if let Some(response) = &mut self.response {
                    response.delta(&mut self.ids, Part::Audio, delta, out);
                    return Ok(());
                }
                (
                    json!({"type": "response.output_audio.delta", "delta": delta}),
                    native,
                )
            }
            Output::Text { text, native } => {
                if let Some(response) = &mut self.response {
                    response.delta(&mut self.ids, Part::Text, text.as_str().into(), out);
                    return Ok(());
                }
                (
                    json!({"type": "response.output_text.delta", "delta": text}),
                    native,
                )
            }
            Output::Transcript { text, native } => {
                if let Some(response) = &mut self.response {
                    let delta = text.as_str().into();
                    response.delta(&mut self.ids, Part::Transcript, delta, out);
                    return Ok(());
                }
                let v = json!({"type": "response.output_audio_transcript.delta", "delta": text});
                (v, native)
            }
            Output::ToolCall {
                id,
                name,
                arguments,
                native,
            } => {
                if let Some(response) = &mut self.response {
                    response.call(&mut self.ids, id, name, arguments, out);
                    return Ok(());
                }
                let v = json!({"type": "response.function_call_arguments.done", "call_id": id,
                    "name": name, "arguments": arguments});
                (v, native)
            }
            Output::Usage(usage) => {
                *self.usage.get_or_insert_default() += usage;
                return Ok(());
            }
            // OpenAI Realtime bills by token: a duration has no place in it.
            Output::Seconds(_) => return Ok(()),
            Output::ResponseDone { status, native } => {
                let (said, reason) = spelled(status).ok_or_else(|| refuse("this status"))?;
                let mut v = json!({"type": "response.done", "response": {"status": said}});
                if let Some(reason) = reason {
                    v["response"]["status_details"] = json!({"type": said, "reason": reason});
                }
                if let Some(mut response) = self.response.take() {
                    response.close(said == "completed", out);
                    let made_up = json!({"object": "realtime.response", "id": response.id,
                        "status_details": null, "output": response.output});
                    merge(
                        &mut v["response"],
                        made_up.as_object().unwrap_or(&Map::new()),
                    );
                }
                merge(&mut v, native.fields::<Key>());
                for (path, counter) in USAGE {
                    let mut usage = self.usage.unwrap_or_default();
                    let n = *counter(&mut usage);
                    if n > 0 {
                        set(&mut v, path, n.into());
                    }
                }
                self.usage = None;
                out.push(v);
                return Ok(());
            }
            Output::Error {
                cause,
                message,
                native,
            } => {
                let mut v = json!({"type": "error", "error": {"message": message}});
                // Its own type it kept; otherwise the cause says one.
                merge(&mut v, native.fields::<Key>());
                if v["error"]["type"].is_null() {
                    v["error"]["type"] = match cause {
                        Cause::Invalid | Cause::NothingToCancel => "invalid_request_error",
                        _ => "server_error",
                    }
                    .into();
                }
                if *cause == Cause::NothingToCancel && native.fields::<Key>().is_empty() {
                    v["error"]["code"] = "response_cancel_not_active".into();
                }
                (v, native)
            }
            Output::Native(native) => match native.value::<Key>() {
                Some(value) => {
                    out.push(value.clone());
                    return Ok(());
                }
                None => return Err(refuse("another format's event")),
            },
            _ => return Err(refuse("another format's event")),
        };
        merge(&mut v, native.fields::<Key>());
        out.push(v);
        Ok(())
    }

    /// The item of the user's turn, made up if no event named it yet.
    fn user(&mut self) -> String {
        let ids = &mut self.ids;
        self.user.get_or_insert_with(|| ids.next("item")).clone()
    }
}

/// Whether `event` is OpenAI's, saying as it came what it says beyond the
/// kind's fields.
fn owned(event: &Output) -> bool {
    let native = match event {
        Output::Opened(s) | Output::Updated(s) => &s.native,
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
        _ => return true,
    };
    !native.fields::<Key>().is_empty()
}

/// The ids a client's writer makes up: a prefix and a number, unique in
/// its session.
#[derive(Debug, Default)]
struct Ids(u64);

impl Ids {
    fn next(&mut self, prefix: &str) -> String {
        self.0 += 1;
        format!("{prefix}_gw{}", self.0)
    }
}

/// A response that began in another format.
#[derive(Debug)]
struct Response {
    id: String,
    /// Its message, while it is being said.
    said: Option<Said>,
    /// Its items said, as its `response.done` lists them.
    output: Vec<Value>,
}

/// The message a response is saying.
#[derive(Debug)]
struct Said {
    id: String,
    /// Whether it is spoken, rather than written.
    audio: bool,
    /// Its transcript, or its text, so far.
    text: String,
}

/// What a delta of a response's message is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    Audio,
    Transcript,
    Text,
}

impl Response {
    /// Where an event of its message is: the response, the item, their
    /// indexes.
    fn at(&self, said: &Said) -> Value {
        json!({"response_id": self.id, "item_id": said.id,
            "output_index": self.output.len(), "content_index": 0})
    }

    /// The next `delta` of its message, which begins with its first.
    fn delta(&mut self, ids: &mut Ids, part: Part, delta: Value, out: &mut Vec<Value>) {
        let mut said = match self.said.take() {
            Some(said) => said,
            None => self.open(ids, part != Part::Text, out),
        };
        let kind = match part {
            Part::Audio => "response.output_audio.delta",
            Part::Transcript => "response.output_audio_transcript.delta",
            Part::Text => "response.output_text.delta",
        };
        if let (Part::Transcript | Part::Text, Some(text)) = (part, delta.as_str()) {
            said.text.push_str(text);
        }
        let mut v = self.at(&said);
        v["type"] = kind.into();
        v["delta"] = delta;
        out.push(v);
        self.said = Some(said);
    }

    /// Its message's item and content part, added.
    fn open(&mut self, ids: &mut Ids, audio: bool, out: &mut Vec<Value>) -> Said {
        let said = Said {
            id: ids.next("item"),
            audio,
            text: String::new(),
        };
        let item = json!({"id": said.id, "object": "realtime.item", "type": "message",
            "status": "in_progress", "role": "assistant", "content": []});
        out.push(
            json!({"type": "response.output_item.added", "response_id": self.id,
            "output_index": self.output.len(), "item": item}),
        );
        out.push(json!({"type": "conversation.item.added", "item": item}));
        let mut v = self.at(&said);
        v["type"] = "response.content_part.added".into();
        v["part"] = match audio {
            true => json!({"type": "audio", "transcript": ""}),
            false => json!({"type": "text", "text": ""}),
        };
        out.push(v);
        said
    }

    /// Its message's end, if it is saying one: `completed` or not.
    fn close(&mut self, completed: bool, out: &mut Vec<Value>) {
        let Some(said) = self.said.take() else {
            return;
        };
        let event = |kind: &str, fields: Value| {
            let mut v = self.at(&said);
            v["type"] = kind.into();
            merge(&mut v, fields.as_object().unwrap_or(&Map::new()));
            v
        };
        let text = said.text.as_str();
        let (part, content) = match said.audio {
            true => {
                out.push(event("response.output_audio.done", json!({})));
                out.push(event(
                    "response.output_audio_transcript.done",
                    json!({"transcript": text}),
                ));
                (
                    json!({"type": "audio", "transcript": text}),
                    json!({"type": "output_audio", "transcript": text}),
                )
            }
            false => {
                out.push(event("response.output_text.done", json!({"text": text})));
                (
                    json!({"type": "text", "text": text}),
                    json!({"type": "output_text", "text": text}),
                )
            }
        };
        out.push(event("response.content_part.done", json!({"part": part})));
        let status = if completed { "completed" } else { "incomplete" };
        let item = json!({"id": said.id, "object": "realtime.item", "type": "message",
            "status": status, "role": "assistant", "content": [content]});
        self.done(item, out);
    }

    /// A call of tool `name`: its item, its arguments whole, its end.
    fn call(&mut self, ids: &mut Ids, id: &str, name: &str, arguments: &str, out: &mut Vec<Value>) {
        self.close(true, out);
        let item_id = ids.next("item");
        let item = |status: &str, arguments: &str| {
            json!({"id": item_id, "object": "realtime.item", "type": "function_call",
                "status": status, "name": name, "call_id": id, "arguments": arguments})
        };
        let index = self.output.len();
        let added = item("in_progress", "");
        out.push(
            json!({"type": "response.output_item.added", "response_id": self.id,
            "output_index": index, "item": added}),
        );
        out.push(json!({"type": "conversation.item.added", "item": added}));
        let at = json!({"response_id": self.id, "item_id": item_id, "output_index": index,
            "call_id": id});
        for (kind, fields) in [
            (
                "response.function_call_arguments.delta",
                json!({"delta": arguments}),
            ),
            (
                "response.function_call_arguments.done",
                json!({"name": name, "arguments": arguments}),
            ),
        ] {
            let mut v = at.clone();
            v["type"] = kind.into();
            merge(&mut v, fields.as_object().unwrap_or(&Map::new()));
            out.push(v);
        }
        self.done(item("completed", arguments), out);
    }

    /// `item` done: told of, the conversation's first, as OpenAI does, and
    /// listed among its items.
    fn done(&mut self, item: Value, out: &mut Vec<Value>) {
        out.push(json!({"type": "conversation.item.done", "item": item}));
        out.push(
            json!({"type": "response.output_item.done", "response_id": self.id,
            "output_index": self.output.len(), "item": item}),
        );
        self.output.push(item);
    }
}
