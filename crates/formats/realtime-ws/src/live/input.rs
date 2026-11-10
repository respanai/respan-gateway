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

/// A session's audio format until it says another.
pub(super) const DEFAULT_FORMAT: AudioFormat = AudioFormat::pcm16(24_000);

/// A client's frames as events: its session's start and updates, and its
/// audio; any other is native.
#[derive(Debug)]
pub(super) struct Client {
    /// The format of the client's audio, as its session says.
    input: AudioFormat,
}

impl Client {
    pub(super) fn new() -> Self {
        Self {
            input: DEFAULT_FORMAT,
        }
    }

    pub(super) fn read(&mut self, raw: Bytes, out: &mut Vec<Input>) -> Result<(), Fault> {
        let v: Value = serde_json::from_slice(&raw)
            .map_err(|_| Fault::bad_request("a client's frame is not a JSON event"))?;
        let mut rest = v.clone();
        let event = match take(&mut rest, &["type"], string, text).as_deref() {
            Some("session.start" | "session.update") => {
                let s = session::decode(rest);
                self.input = s.input_audio.unwrap_or(self.input);
                Some(Input::Session(s))
            }
            Some("session.input_audio.append") => {
                json::take_audio(&mut rest, &["audio"]).map(|data| Input::Audio {
                    audio: Audio {
                        data,
                        format: self.input,
                    },
                    native: json::native(NAME, rest),
                })
            }
            _ => None,
        };
        out.push(event.unwrap_or_else(|| Input::native(NAME, v)));
        Ok(())
    }
}

/// The client's events written to the provider: its first session as
/// `session.start`, which fixes the session, and its audio.
#[derive(Debug)]
pub(super) struct Provider {
    /// The model routing chose, else the one at the client's `?model=`.
    model: Option<String>,
    say: Say,
    /// The session as it was started, once it was.
    started: Option<Session>,
    /// The session as the client asked it, which a later one is judged by.
    asked: Option<Session>,
    /// The format of the audio it takes, as its start said.
    input: AudioFormat,
    convert: Convert,
}

impl Provider {
    pub(super) fn new(model: Option<String>, say: Say) -> Self {
        Self {
            model,
            say,
            started: None,
            asked: None,
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
        let refuse = |what: &str| Err(untranslatable_input(NAME, what));
        match event {
            Input::Session(s) if self.started.is_none() => self.start(s).map(Some),
            Input::Session(s) => self.update(s),
            _ if self.started.is_none() => Err(Fault::internal(
                "an OpenAI Live session begins with its start",
            )),
            Input::Audio { audio, native } => {
                let Some(data) = self.convert.to(audio, self.input) else {
                    return refuse("audio of this format");
                };
                let mut v =
                    json!({"type": "session.input_audio.append", "audio": json::base64(&data)});
                merge(&mut v, own(native));
                Ok(Some(v))
            }
            Input::Native(NativeValue { format, value }) if **format == *NAME => {
                Ok(Some(Value::clone(value)))
            }
            // Commits, messages, tools' answers and responses: it has none.
            _ => refuse("this event"),
        }
    }

    /// The session's start: the model routing chose, else the handshake's,
    /// which routing judged, else its own; the client's audio format if the
    /// Live API takes it, else its own, which the audio is converted to.
    /// The provider's voice detection and transcription, which it always
    /// does, are dropped.
    fn start(&mut self, s: &Session) -> Result<Value, Fault> {
        self.asked = Some(s.clone());
        let mut s = s.clone();
        s.model = self.model.take().or(s.model);
        let unsupported = [
            (s.model.is_none(), "a session naming no model"),
            (s.purpose == Some(Purpose::Transcription), "a transcription"),
            (s.output == Some(Modality::Text), "text output"),
            (s.turns == Some(Turns::Manual), "turns the client ends"),
            (s.tools.as_ref().is_some_and(|t| !t.is_empty()), "tools"),
            (s.max_output_tokens.is_some(), "a limit on output tokens"),
        ];
        if let Some((_, what)) = unsupported.iter().find(|(refused, _)| *refused) {
            return Err(untranslatable_input(NAME, what));
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

    /// A later session: the Live client's own update, as it came; else one
    /// that changes nothing the client asked, acknowledged with the session
    /// as it was started, as the rest is fixed at its start.
    fn update(&mut self, s: &Session) -> Result<Option<Value>, Fault> {
        let mut later = s.clone();
        let native = later.native.take();
        let asked = self.asked.clone().unwrap_or_default();
        if let Some(field) = changes(&asked, &later) {
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
        let started = self.started.clone().unwrap_or_default();
        let _ = self.say.unbounded_send(Output::Updated(started));
        Ok(None)
    }
}
