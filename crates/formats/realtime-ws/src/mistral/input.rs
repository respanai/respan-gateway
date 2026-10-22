//! What the client says: Mistral's client events as the kind's [`Input`],
//! read at the client's edge, and the kind's written to Mistral's session.

use respan_gateway_core::{Fault, NativeFields, NativeValue};
use respan_gateway_http::Encoder;
use respan_gateway_http::bytes::Bytes;
use respan_gateway_realtime::{Audio, AudioFormat, Input, Purpose, Session, Turns};
use serde_json::{Value, json};

use super::{NAME, session};
use crate::audio::Convert;
use crate::foreign;
use crate::json::{self, merge, own, string, take, text, untranslatable_input};

/// The most audio one append carries, in bytes.
const APPEND: usize = 262_144;

/// The audio a session takes until one says another.
const AUDIO: AudioFormat = AudioFormat::pcm16(16_000);

/// A client's frames as events, beginning with the session its handshake
/// asked for: a transcription, its model the one at `?model=`, its turns
/// ended by the client, as Mistral detects no voices.
#[derive(Debug)]
pub(super) struct Client {
    /// The model the handshake asked for.
    model: Option<String>,
    /// Whether its session was opened.
    opened: bool,
    /// The format of the client's audio, as its session says.
    input: AudioFormat,
}

impl Client {
    pub(super) fn new(model: Option<String>) -> Self {
        Self {
            model,
            opened: false,
            input: AUDIO,
        }
    }

    pub(super) fn read(&mut self, raw: Bytes, out: &mut Vec<Input>) -> Result<(), Fault> {
        let v: Value = serde_json::from_slice(&raw)
            .map_err(|_| Fault::bad_request("a client's frame is not a JSON event"))?;
        let event = self.decode(v)?;
        if std::mem::replace(&mut self.opened, true) {
            out.push(event);
            return Ok(());
        }
        let (first, rest) = match event {
            Input::Session(s) => (s, None),
            other => (Session::default(), Some(other)),
        };
        out.push(Input::Session(open(first, self.model.take(), self.input)));
        out.extend(rest);
        Ok(())
    }

    fn decode(&mut self, mut v: Value) -> Result<Input, Fault> {
        let rest = &mut v;
        let kind = take(rest, &["type"], string, text).unwrap_or_default();
        let event = match kind.as_str() {
            "session.update" => {
                let s = session::decode(std::mem::take(rest));
                if let Some(format) = s.input_audio {
                    self.input = format;
                }
                return Ok(Input::Session(s));
            }
            "input_audio.append" => {
                let data = json::take_audio(rest, &["audio"])
                    .ok_or_else(|| Fault::bad_request("an append's audio is not base64"))?;
                Input::Audio {
                    audio: Audio {
                        data,
                        format: self.input,
                    },
                    native: json::native(NAME, std::mem::take(rest)),
                }
            }
            // The end of its audio makes the transcript whole, as a flush
            // does.
            "input_audio.flush" | "input_audio.end" => Input::Commit {
                native: json::native(NAME, std::mem::take(rest)),
            },
            _ => {
                rest["type"] = kind.into();
                return Ok(Input::native(NAME, v));
            }
        };
        Ok(event)
    }
}

/// `s`, a client's first session, as the transcription it is: of `model`
/// and in `audio` unless it names others, its turns the client's.
fn open(mut s: Session, model: Option<String>, audio: AudioFormat) -> Session {
    s.model = s.model.or(model);
    s.input_audio = s.input_audio.or(Some(audio));
    s.purpose = Some(Purpose::Transcription);
    s.input_transcription = Some(true);
    s.turns = Some(Turns::Manual);
    s
}

/// The client's events written to the provider.
#[derive(Debug)]
pub(super) struct Provider {
    /// The model the session was opened with, at its URL.
    model: String,
    /// Whether its first session was written.
    opened: bool,
    /// The format of the audio it takes, as the sessions written say.
    input: AudioFormat,
    /// The client's audio in that format.
    convert: Convert,
    /// Whether audio was sent since the last commit.
    heard: bool,
}

impl Provider {
    /// A writer to a session opened with `model`.
    pub(super) fn new(model: String) -> Self {
        Self {
            model,
            opened: false,
            input: AUDIO,
            convert: Convert::default(),
            heard: false,
        }
    }

    pub(super) fn write(&mut self, event: &Input, out: &mut Vec<Bytes>) -> Result<(), Fault> {
        let event = foreign::admit(NAME, event)?;
        for v in self.frames(&event)? {
            out.push(json::frame(&v)?);
        }
        Ok(())
    }

    fn frames(&mut self, event: &Input) -> Result<Vec<Value>, Fault> {
        let refuse = |what: &str| untranslatable_input(NAME, what);
        let own_of = |native: &Option<NativeFields>| {
            own(NAME, native, || refuse("another format's event")).map(|own| own.cloned())
        };
        let frames = match event {
            Input::Session(s) => return self.session(s).map(|v| v.into_iter().collect()),
            Input::Audio { audio, native } => {
                let data = self.convert.to(audio, self.input);
                let data = data.ok_or_else(|| refuse("audio of this format"))?;
                self.heard = true;
                let mut native = own_of(native)?;
                let mut frames = Vec::new();
                for chunk in data.chunks(APPEND) {
                    let mut v = json!({"type": "input_audio.append", "audio": json::base64(chunk)});
                    merge(&mut v, native.take().as_ref());
                    frames.push(v);
                }
                frames
            }
            Input::Commit { native } => {
                self.heard = false;
                let mut v = json!({"type": "input_audio.flush"});
                merge(&mut v, own_of(native)?.as_ref());
                vec![v]
            }
            Input::Native(NativeValue { format, value }) if **format == *NAME => {
                vec![Value::clone(value)]
            }
            Input::Native(native) => {
                foreign::native_input(NAME, native, self.heard)?;
                Vec::new()
            }
            _ => return Err(refuse("this event")),
        };
        Ok(frames)
    }

    /// A session as its update, if it says anything to update.
    fn session(&mut self, s: &Session) -> Result<Option<Value>, Fault> {
        let refuse = |what: &str| untranslatable_input(NAME, what);
        // Mistral takes its model at connect: the first session's is the
        // one at its URL, and a later one that names it again changes
        // nothing.
        let opening = !std::mem::replace(&mut self.opened, true);
        let mut s = s.clone();
        if opening || s.model.as_ref() == Some(&self.model) {
            s.model = None;
        }
        if s.model.is_some() {
            return Err(refuse("a change of model"));
        }
        // Its voice detection does not change what it transcribes.
        s.turns = None;
        s.output_transcription = None;
        s.input_transcription = s.input_transcription.filter(|on| !on);
        let session = session::encode(&s, refuse)?;
        let mut v = json!({"type": "session.update", "session": session});
        merge(
            &mut v,
            own(NAME, &s.native, || refuse("another format's session"))?,
        );
        if let Some(format) = s.input_audio {
            self.input = format;
        }
        let empty = v["session"]
            .as_object()
            .is_none_or(|fields| fields.is_empty());
        Ok((!empty).then_some(v))
    }
}

/// The values of the events, and at their end, the end of the audio.
impl Encoder<Input> for Provider {
    fn encode(&mut self, event: &Input, out: &mut Vec<Bytes>) -> Result<(), Fault> {
        self.write(event, out)
    }

    fn end(&mut self, end: Result<(), Fault>, out: &mut Vec<Bytes>) -> Result<(), Fault> {
        if end.is_ok() {
            out.push(json::frame(&json!({"type": "input_audio.end"}))?);
        }
        end
    }
}
