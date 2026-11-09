//! What the client says: the Live API's client events as the kind's
//! [`Input`], read at the client's edge and written at the provider's.

use respan_gateway_core::{Fault, NativeValue};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_realtime::{
    Audio, AudioFormat, Input, Modality, Output, Purpose, Session, Turns,
};
use respan_gateway_ws::CloseCode;
use serde_json::{Value, json};

use super::{NAME, session};
use crate::audio::Convert;
use crate::foreign;
use crate::json::{self, merge, own, string, take, text, untranslatable_input};
use crate::session::{Say, changes};

/// The format of a session's audio until it says another, as the Live API
/// has it.
pub(super) const DEFAULT_FORMAT: AudioFormat = AudioFormat::pcm16(24_000);

/// A client's frames as events, beginning with its session: its
/// `session.start`, naming the model at `?model=` if it names none, or, if
/// its first frame is another event, a session of that model alone before
/// it.
#[derive(Debug)]
pub(super) struct Client {
    /// The model the handshake asked for, until the first frame.
    opening: Option<Option<String>>,
    /// The format of the client's audio, as its session says.
    input: AudioFormat,
}

impl Client {
    pub(super) fn new(model: Option<String>) -> Self {
        Self {
            opening: Some(model),
            input: DEFAULT_FORMAT,
        }
    }

    pub(super) fn read(&mut self, raw: Bytes, out: &mut Vec<Input>) -> Result<(), Fault> {
        let v: Value = serde_json::from_slice(&raw)
            .map_err(|_| Fault::bad_request("a client's frame is not a JSON event"))?;
        let mut event = decode(v, self.input);
        if let Some(model) = self.opening.take() {
            match &mut event {
                Input::Session(s) => s.model = s.model.take().or(model),
                _ => {
                    let mut opening = Session::default();
                    opening.model = model;
                    out.push(Input::Session(opening));
                }
            }
        }
        if let Input::Session(Session {
            input_audio: Some(format),
            ..
        }) = &event
        {
            self.input = *format;
        }
        out.push(event);
        Ok(())
    }
}

/// A client event as the kind's, its audio in `input`: its session's start
/// and updates, and its audio. One the kind has no form for, or one it
/// could not write back as it came, is native.
fn decode(v: Value, input: AudioFormat) -> Input {
    let mut rest = v.clone();
    let kind = take(&mut rest, &["type"], string, text);
    let event = match kind.as_deref() {
        Some("session.start" | "session.update") => Some(Input::Session(session::decode(rest))),
        Some("session.input_audio.append") => {
            json::take_audio(&mut rest, &["audio"]).map(|data| Input::Audio {
                audio: Audio {
                    data,
                    format: input,
                },
                native: json::native(NAME, rest),
            })
        }
        _ => None,
    };
    event.unwrap_or_else(|| Input::native(NAME, v))
}

/// The client's events written to the provider: its first session as
/// `session.start`, which fixes the session, and its audio.
///
/// What only another format says is dropped, emulated or refused: the
/// provider's voice detection and transcription, which the Live API always
/// does, are dropped; a later session that changes nothing is acknowledged,
/// and one that changes anything refused; a transcription, text output,
/// turns the client ends, tools, a limit on output tokens, a commit, a
/// message, a tool's answer, and a response to make or cancel are refused,
/// as it has none of them.
#[derive(Debug)]
pub(super) struct Provider {
    /// The model routing chose.
    chosen: Option<String>,
    /// The model the client's handshake asked for.
    asked: Option<String>,
    say: Say,
    /// The session as it was started, once it was.
    started: Option<Session>,
    /// The format of the audio it takes, as its start said.
    input: AudioFormat,
    /// The client's audio in that format.
    convert: Convert,
}

impl Provider {
    pub(super) fn new(chosen: Option<String>, asked: Option<String>, say: Say) -> Self {
        Self {
            chosen,
            asked,
            say,
            started: None,
            input: DEFAULT_FORMAT,
            convert: Convert::default(),
        }
    }

    pub(super) fn write(&mut self, event: &Input, out: &mut Vec<Bytes>) -> Result<(), Fault> {
        let Some(event) = foreign::admit(NAME, event) else {
            return Ok(());
        };
        if let Some(v) = self.encode(&event)? {
            out.push(json::frame(&v)?);
        }
        Ok(())
    }

    fn encode(&mut self, event: &Input) -> Result<Option<Value>, Fault> {
        let refuse = |what: &str| untranslatable_input(NAME, what);
        let (mut v, native) = match event {
            Input::Session(s) if self.started.is_none() => return self.start(s).map(Some),
            Input::Session(s) => return self.update(s),
            _ if self.started.is_none() => {
                return Err(Fault::internal(
                    "an OpenAI Live session begins with its start",
                ));
            }
            Input::Audio { audio, native } => {
                let data = self.convert.to(audio, self.input);
                let data = data.ok_or_else(|| refuse("audio of this format"))?;
                let v = json!({"type": "session.input_audio.append", "audio": json::base64(&data)});
                (v, native)
            }
            Input::Commit { .. } => return Err(refuse("a commit of the user's turn")),
            Input::Message { .. } => return Err(refuse("a message")),
            Input::ToolResult(_) => return Err(refuse("a tool's answer")),
            Input::Respond { .. } => return Err(refuse("a response to make")),
            Input::Cancel { .. } => return Err(refuse("a response to cancel")),
            Input::Native(NativeValue { format, value }) if **format == *NAME => {
                return Ok(Some(Value::clone(value)));
            }
            _ => return Err(refuse("another format's event")),
        };
        merge(&mut v, own(native));
        Ok(Some(v))
    }

    /// The session's start, naming the model routing chose, else the one
    /// the handshake asked for, which routing judged, else its own; its
    /// audio in the client's format if the Live API takes it, else in its
    /// own.
    fn start(&mut self, s: &Session) -> Result<Value, Fault> {
        let refuse = |what: &str| untranslatable_input(NAME, what);
        let mut s = s.clone();
        s.model = (self.chosen.take().or_else(|| self.asked.take())).or(s.model);
        if s.model.is_none() {
            return Err(Fault::bad_request(
                "OpenAI Live names its model in its session's start: name it there, or with ?model= on the session's URL",
            ));
        }
        if s.purpose == Some(Purpose::Transcription) {
            return Err(refuse("a transcription session"));
        }
        if s.output == Some(Modality::Text) {
            return Err(refuse("text output"));
        }
        if s.turns == Some(Turns::Manual) {
            return Err(refuse("turns the client ends"));
        }
        if s.tools.as_ref().is_some_and(|tools| !tools.is_empty()) {
            return Err(refuse("tools"));
        }
        if s.max_output_tokens.is_some() {
            return Err(refuse("a limit on output tokens"));
        }
        let format = [s.input_audio, s.output_audio]
            .into_iter()
            .flatten()
            .find(|f| session::takes(*f));
        self.input = format.unwrap_or(DEFAULT_FORMAT);
        let mut v = json!({"type": "session.start", "session": session::encode(&s, format)});
        merge(&mut v, own(&s.native));
        self.started = Some(s);
        Ok(v)
    }

    /// A later session: the Live client's own update, written as it came;
    /// else one that changes nothing, acknowledged, as the Live API fixes
    /// the rest of a session at its start.
    fn update(&mut self, s: &Session) -> Result<Option<Value>, Fault> {
        let started = self.started.get_or_insert_default();
        let mut asked = s.clone();
        let native = asked.native.take();
        if let Some(field) = changes(started, &asked) {
            let fixed = format!(
                "OpenAI Live fixes a session's configuration at its start: this update changes `{field}`"
            );
            return Err(Fault::bad_request(fixed).with(CloseCode(1008)));
        }
        if let Some(native) = native {
            let mut v = json!({"type": "session.update"});
            merge(&mut v, Some(&native.fields));
            return Ok(Some(v));
        }
        let _ = self.say.unbounded_send(Output::Updated(started.clone()));
        Ok(None)
    }
}
