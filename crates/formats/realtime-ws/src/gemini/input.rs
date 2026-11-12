//! What the client says: the Live API's client messages as the kind's
//! [`Input`], read at the client's edge and written at the provider's.

use std::sync::atomic::Ordering;

use respan_gateway_conversation::{Content, Message, Role};
use respan_gateway_core::{Fault, NativeFields, NativeValue};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_realtime::{
    Audio, AudioFormat, Encoding, Input, Output, Session, Status, ToolResult, Turns,
};
use respan_gateway_ws::CloseCode;
use serde_json::{Map, Value, json};

use super::{Answering, NAME, Shared, setup};
use crate::audio::Convert;
use crate::foreign;
use crate::json::{self, merge, own, untranslatable_input};
use crate::session::{self, Say};

/// A client's messages as events: its `setup` first and once, and each
/// message one of `setup`, `clientContent`, `realtimeInput` and
/// `toolResponse`, as the Live API takes them; any other is the client's
/// fault, closed 1007.
///
/// Its `activityStart` is the user's turn beginning: a [`Input::Cancel`]
/// while it is answered, as the Live API stops what it says, else native.
#[derive(Debug)]
pub(super) struct Client {
    setup: bool,
    answering: Answering,
}

impl Client {
    pub(super) fn new(answering: Answering) -> Self {
        Self {
            setup: false,
            answering,
        }
    }

    pub(super) fn read(&mut self, raw: Bytes, out: &mut Vec<Input>) -> Result<(), Fault> {
        let v: Value =
            serde_json::from_slice(&raw).map_err(|_| invalid("a client's message is not JSON"))?;
        let field = v
            .as_object()
            .filter(|m| m.len() == 1)
            .and_then(|m| m.keys().next());
        let field = field.ok_or_else(|| {
            invalid("a message holds one of setup, clientContent, realtimeInput and toolResponse")
        })?;
        match (field.as_str(), self.setup) {
            ("setup", false) => {
                self.setup = true;
                out.push(Input::Session(setup::decode(v)));
            }
            ("setup", true) => return Err(invalid("setup is sent once")),
            (_, false) => return Err(invalid("a session's first message is its setup")),
            _ => {
                let answering = self.answering.load(Ordering::Relaxed);
                out.extend(decode(&v, answering).unwrap_or_else(|| vec![Input::native(NAME, v)]));
            }
        }
        Ok(())
    }
}

/// A message the Live API refuses, as it closes the session.
fn invalid(message: &str) -> Fault {
    Fault::bad_request(message).with(CloseCode(1007))
}

/// The events of a message after the setup, its client `answering` or not;
/// `None` for one the kind has no form for, or one its events would not
/// write back as it came.
fn decode(v: &Value, answering: bool) -> Option<Vec<Input>> {
    let events = match v.as_object()?.iter().next()? {
        (name, body) if name == "realtimeInput" => {
            let (field, value) = single(body)?;
            match field {
                "audio" => return audio(value).map(|a| vec![a]),
                "text" => vec![user(value.as_str()?), Input::Respond { native: None }],
                "activityEnd" if *value == json!({}) => vec![
                    Input::Commit { native: None },
                    Input::Respond { native: None },
                ],
                "activityStart" if answering && *value == json!({}) => vec![Input::Cancel {
                    native: json::native(NAME, v.clone()),
                }],
                _ => return None,
            }
        }
        (name, body) if name == "clientContent" => {
            let turns = match body.get("turns") {
                Some(turns) => turns.as_array()?.as_slice(),
                None => &[],
            };
            let mut events = Vec::new();
            for turn in turns {
                events.push(Input::Message {
                    message: message(turn)?,
                    native: None,
                });
            }
            // The last event of the message keeps its shape, and ends it.
            let shape = json::native(NAME, json!({"clientContent": {}}));
            match (body["turnComplete"] == true, events.last_mut()) {
                (true, _) => events.push(Input::Respond { native: shape }),
                (false, Some(Input::Message { native, .. })) => *native = shape,
                (false, _) => return None,
            }
            events
        }
        (name, body) if name == "toolResponse" => {
            let answers = body["functionResponses"].as_array()?;
            let mut events = answers.iter().map(answer).collect::<Option<Vec<_>>>()?;
            events.push(Input::Respond { native: None });
            events
        }
        _ => return None,
    };
    let mut provider = Provider::past_setup();
    let mut written = Vec::new();
    for event in &events {
        provider.encode(event, &mut written).ok()?;
    }
    provider.flush(&mut written);
    (written.as_slice() == std::slice::from_ref(v)).then_some(events)
}

/// The one field of `v`, an object.
fn single(v: &Value) -> Option<(&str, &Value)> {
    match v.as_object()? {
        fields if fields.len() == 1 => fields.iter().next().map(|(k, v)| (k.as_str(), v)),
        _ => None,
    }
}

/// A chunk of the user's audio: PCM at the rate its type declares.
fn audio(blob: &Value) -> Option<Input> {
    let mut rest = blob.clone();
    let rate = rest["mimeType"]
        .as_str()?
        .strip_prefix("audio/pcm;rate=")?
        .parse()
        .ok()?;
    let data = json::take_audio(&mut rest, &["data"])?;
    rest.as_object_mut()?.shift_remove("mimeType");
    let native = match rest.as_object()?.is_empty() {
        true => None,
        false => json::native(NAME, json!({"realtimeInput": {"audio": rest}})),
    };
    Some(Input::Audio {
        audio: Audio {
            data,
            format: AudioFormat::pcm16(rate),
        },
        native,
    })
}

/// The user's text, as a message.
fn user(text: &str) -> Input {
    Input::Message {
        message: Message::new(Role::User, text),
        native: None,
    }
}

/// A turn of `clientContent`: a role and parts; a part of text alone is the
/// kind's, any other stays the Live API's.
fn message(turn: &Value) -> Option<Message> {
    let mut rest = turn.as_object()?.clone();
    let role = match rest.shift_remove("role")?.as_str()? {
        "user" => Role::User,
        "model" => Role::Assistant,
        _ => return None,
    };
    let parts = rest.shift_remove("parts")?;
    let part = |p: &Value| match p.as_object().map(|o| (o.len(), o.get("text"))) {
        Some((1, Some(Value::String(text)))) => Content::from(text.as_str()),
        _ => Content::native(NAME, p.clone()),
    };
    let mut message = Message::new(role, "");
    message.content = parts.as_array()?.iter().map(part).collect();
    message.native = json::native(NAME, Value::Object(rest));
    Some(message)
}

fn message_json(m: &Message) -> Value {
    // WARN: Gemini Live's turns are the user's or the model's, so a message
    // of another role, such as a system message, is the user's.
    let role = if m.role == Role::Assistant {
        "model"
    } else {
        "user"
    };
    // WARN: a part but text, such as an image, is dropped.
    let part = |c: &Content| match c {
        Content::Text { text, native, .. } if native.is_none() => Some(json!({"text": text})),
        Content::Native(NativeValue { format, value }) if **format == *NAME => {
            Some(Value::clone(value))
        }
        _ => None,
    };
    let parts: Vec<Value> = m.content.iter().filter_map(part).collect();
    let mut v = json!({"role": role, "parts": parts});
    merge(&mut v, own(&m.native));
    v
}

/// A function's answer: its response, as JSON text, and its other fields,
/// such as its name, kept.
fn answer(v: &Value) -> Option<Input> {
    let mut rest = v.as_object()?.clone();
    let id = rest.shift_remove("id")?.as_str()?.to_owned();
    let response = rest.shift_remove("response")?;
    let mut result = ToolResult::new(id, response.to_string());
    result.native = json::native(NAME, Value::Object(rest));
    Some(Input::ToolResult(result))
}

/// A tool's answer as a function response: its output as the response if
/// it is a JSON object, else the object `{"output": ..}`, and the `name` of
/// the tool it answers if it does not say it.
fn answer_json(result: &ToolResult, name: Option<&String>) -> Value {
    let response = match serde_json::from_str::<Value>(&result.output) {
        Ok(object @ Value::Object(_)) => object,
        _ => json!({"output": result.output}),
    };
    let mut v = json!({"id": result.id, "response": response});
    if let (None, Some(name)) = (&result.native, name) {
        v["name"] = name.as_str().into();
    }
    merge(&mut v, own(&result.native));
    v
}

/// What the client says that the next [`Input::Respond`] answers.
#[derive(Debug)]
enum Held {
    /// Messages, which the Live API adds to the conversation together.
    Messages(Vec<Input>),
    /// Tools' answers, which it takes together.
    Results(Vec<ToolResult>),
}

/// The client's events written to the provider: its setup first, then
/// messages and tools' answers held until what answers them, so that a
/// message writes them together.
///
/// What only another format says is emulated: with manual turns, the
/// user's activity begins at its first audio; a cancel ends the response
/// at once, and the rest of its turn is dropped; a later session is
/// acknowledged. A tool's answer to a call the provider cancelled is not
/// written at all.
#[derive(Debug)]
pub(super) struct Provider {
    shared: Shared,
    say: Say,
    /// Whether the setup was written.
    opened: bool,
    held: Option<Held>,
    /// Whether the last message already makes the model answer.
    answered: bool,
    /// Whether the user's activity is open, with manual turns.
    activity: bool,
    /// Its audio in PCM.
    convert: Convert,
}

impl Provider {
    pub(super) fn new(shared: Shared, say: Say) -> Self {
        Self {
            shared,
            say,
            opened: false,
            held: None,
            answered: false,
            activity: false,
            convert: Convert::default(),
        }
    }

    /// A writer past a setup of turns the client ends, the user's activity
    /// open: it writes each message of the client's as it came.
    fn past_setup() -> Self {
        let mut session = Session::default();
        session.turns = Some(Turns::Manual);
        let shared = Shared::default();
        shared.lock().applied = Some(session);
        let mut provider = Self::new(shared, session::say().0);
        (provider.opened, provider.activity) = (true, true);
        provider
    }

    pub(super) fn write(&mut self, event: &Input, out: &mut Vec<Bytes>) -> Result<(), Fault> {
        let Some(event) = foreign::admit(NAME, event) else {
            return Ok(());
        };
        let mut messages = Vec::new();
        self.encode(&event, &mut messages)?;
        for message in &messages {
            out.push(json::frame(message)?);
        }
        Ok(())
    }

    fn encode(&mut self, event: &Input, out: &mut Vec<Value>) -> Result<(), Fault> {
        let refuse = |what: &str| untranslatable_input(NAME, what);
        let answered = std::mem::take(&mut self.answered);
        let (mut v, native) = match event {
            Input::Session(s) if !self.opened => {
                self.opened = true;
                out.push(setup::encode(s, refuse)?);
                self.shared.lock().applied = Some(s.clone());
                return Ok(());
            }
            Input::Session(s) => return self.update(s),
            _ if !self.opened => {
                return Err(Fault::internal(
                    "a Gemini Live session begins with its setup",
                ));
            }
            Input::Message { native, .. } => {
                match &mut self.held {
                    Some(Held::Messages(held)) => held.push(event.clone()),
                    _ => {
                        self.flush(out);
                        self.held = Some(Held::Messages(vec![event.clone()]));
                    }
                }
                if shaped(native)
                    && let Some(Held::Messages(messages)) = self.held.take()
                {
                    out.push(messages_json(&messages));
                }
                return Ok(());
            }
            Input::ToolResult(result) if self.shared.lock().withdrawn.contains(&result.id) => {
                // The provider takes no answer to a call it cancelled, nor
                // asks for one.
                self.answered = true;
                return Ok(());
            }
            Input::ToolResult(result) => {
                match &mut self.held {
                    Some(Held::Results(held)) => held.push(result.clone()),
                    _ => {
                        self.flush(out);
                        self.held = Some(Held::Results(vec![result.clone()]));
                    }
                }
                return Ok(());
            }
            Input::Respond { .. } if answered && self.held.is_none() => return Ok(()),
            Input::Respond { native } => (self.respond(native), native),
            _ => {
                self.flush(out);
                let manual = self.manual();
                match event {
                    Input::Audio { audio, native } => {
                        let pcm = AudioFormat::pcm16(audio.format.rate);
                        let data = match audio.format.encoding {
                            Encoding::Pcm16 => Some(audio.data.clone()),
                            _ => self.convert.to(audio, pcm),
                        };
                        let data = data.ok_or_else(|| refuse("audio of this format"))?;
                        if manual && !std::mem::replace(&mut self.activity, true) {
                            out.push(json!({"realtimeInput": {"activityStart": {}}}));
                        }
                        let mime = format!("audio/pcm;rate={}", pcm.rate);
                        let blob = json!({"mimeType": mime, "data": json::base64(&data)});
                        (json!({"realtimeInput": {"audio": blob}}), native)
                    }
                    // WARN: the provider's voice detection ends the user's
                    // turns, so a commit under it is dropped.
                    Input::Commit { .. } if !manual => return Ok(()),
                    Input::Commit { native } => {
                        (self.activity, self.answered) = (false, true);
                        (json!({"realtimeInput": {"activityEnd": {}}}), native)
                    }
                    Input::Cancel { native } if native.is_some() => {
                        self.activity = true;
                        (json!({}), native)
                    }
                    Input::Cancel { .. } => return self.cancel(manual, out),
                    Input::Native(NativeValue { format, value }) if **format == *NAME => {
                        if at_start(value) {
                            self.activity = true;
                        }
                        out.push(Value::clone(value));
                        return Ok(());
                    }
                    _ => return Err(refuse("another format's event")),
                }
            }
        };
        merge(&mut v, own(native));
        out.push(v);
        Ok(())
    }

    /// Whether the client ends the user's turns, as the setup says.
    fn manual(&self) -> bool {
        let state = self.shared.lock();
        let turns = state.applied.as_ref().and_then(|s| s.turns.as_ref());
        matches!(turns, Some(Turns::Manual))
    }

    /// A later session, which the Live API cannot take: acknowledged with
    /// the session as it was set up.
    fn update(&mut self, s: &Session) -> Result<(), Fault> {
        let mut state = self.shared.lock();
        let applied = state.applied.get_or_insert_default();
        // WARN: Gemini Live fixes a session at its setup, so what a later
        // one changes is dropped, but its audio formats, each chunk's own.
        applied.input_audio = s.input_audio.or(applied.input_audio);
        applied.output_audio = s.output_audio.or(applied.output_audio);
        let _ = self.say.unbounded_send(Output::Updated(applied.clone()));
        Ok(())
    }

    /// A cancel, which the Live API has none of: the response being made
    /// ends cancelled, and the rest of its turn is dropped. With manual
    /// turns the user's activity begins, which stops it.
    fn cancel(&mut self, manual: bool, out: &mut Vec<Value>) -> Result<(), Fault> {
        let mut state = self.shared.lock();
        if std::mem::take(&mut state.open) {
            state.cancelled = true;
            let done = Output::ResponseDone {
                status: Status::Cancelled,
                native: None,
            };
            let _ = self.say.unbounded_send(done);
        }
        if manual && !std::mem::replace(&mut self.activity, true) {
            out.push(json!({"realtimeInput": {"activityStart": {}}}));
        }
        Ok(())
    }

    /// The message that asks for an answer to what is held: the user's
    /// text alone unless `native` says it came as `clientContent`, else
    /// messages as a complete `clientContent` turn; tools' answers; or a
    /// complete turn of nothing more.
    fn respond(&mut self, native: &Option<NativeFields>) -> Value {
        let messages = match self.held.take() {
            Some(Held::Results(results)) => return self.results(&results),
            Some(Held::Messages(messages)) => messages,
            None => Vec::new(),
        };
        if let [
            Input::Message {
                message,
                native: None,
            },
        ] = messages.as_slice()
            && !shaped(native)
            && said(message)
        {
            return json!({"realtimeInput": {"text": message.text()}});
        }
        let mut v = match messages.as_slice() {
            [] => json!({"clientContent": {}}),
            _ => messages_json(&messages),
        };
        v["clientContent"]["turnComplete"] = true.into();
        v
    }

    fn results(&self, results: &[ToolResult]) -> Value {
        let calls = &self.shared.lock().calls;
        let answers = results.iter().map(|r| answer_json(r, calls.get(&r.id)));
        let answers: Vec<Value> = answers.collect();
        json!({"toolResponse": {"functionResponses": answers}})
    }

    /// Write what is held, which nothing answers: tools' answers, or
    /// messages as a `clientContent` turn not yet complete.
    fn flush(&mut self, out: &mut Vec<Value>) {
        match self.held.take() {
            None => {}
            Some(Held::Messages(messages)) => out.push(messages_json(&messages)),
            Some(Held::Results(results)) => out.push(self.results(&results)),
        }
    }
}

/// Whether `message` is the user's text alone.
fn said(message: &Message) -> bool {
    message.native.is_none()
        && message.role == Role::User
        && message
            .content
            .iter()
            .all(|c| matches!(c, Content::Text { native: None, .. }))
}

/// Whether `event` begins the user's activity.
fn at_start(event: &Value) -> bool {
    json::at(event, &["realtimeInput", "activityStart"]).is_some()
}

/// Whether an event's `native` says it came as `clientContent`.
fn shaped(native: &Option<NativeFields>) -> bool {
    native
        .as_ref()
        .is_some_and(|n| n.fields.contains_key("clientContent"))
}

/// Messages as one `clientContent`, with what each event kept.
fn messages_json(messages: &[Input]) -> Value {
    let mut turns = Vec::new();
    let mut kept = Map::new();
    for event in messages {
        if let Input::Message { message, native } = event {
            turns.push(message_json(message));
            if let Some(fields) = own(native) {
                kept.extend(fields.clone());
            }
        }
    }
    let mut v = json!({"clientContent": {"turns": turns}});
    merge(&mut v, Some(&kept));
    v
}
