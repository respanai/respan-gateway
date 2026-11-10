//! What the client says: Phonic's client messages as the kind's [`Input`],
//! read at the client's edge and written at the provider's.

use respan_gateway_core::{Fault, NativeValue, Unsupported, Why};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_realtime::{
    Audio, AudioFormat, Input, Modality, Output, Purpose, Session, Turns,
};
use serde_json::{Value, json};

use super::{Applied, Asked, DEFAULT_FORMAT, NAME, session};
use crate::audio::Convert;
use crate::foreign;
use crate::json::{self, merge, own, string, take, text};
use crate::session::{Say, changes};

/// The longest audio chunk Phonic takes, in milliseconds.
const CHUNK_MS: usize = 40;

/// A client's frames as events: its `config` and its audio; any other is
/// native.
#[derive(Debug)]
pub(super) struct Client {
    /// The format of the client's audio, as its `config` says.
    input: AudioFormat,
    /// The format of the audio it takes, which its `config` says.
    asked: Asked,
}

impl Client {
    pub(super) fn new(asked: Asked) -> Self {
        Self {
            input: DEFAULT_FORMAT,
            asked,
        }
    }

    pub(super) fn read(&mut self, raw: Bytes, out: &mut Vec<Input>) -> Result<(), Fault> {
        let v: Value = serde_json::from_slice(&raw)
            .map_err(|_| Fault::bad_request("a client's frame is not a JSON event"))?;
        let mut rest = v.clone();
        let event = match take(&mut rest, &["type"], string, text).as_deref() {
            Some("config") => {
                let s = session::decode(rest);
                self.input = s.input_audio.unwrap_or(self.input);
                *self.asked.lock() = s.output_audio.unwrap_or(DEFAULT_FORMAT);
                Some(Input::Session(s))
            }
            Some("audio_chunk") => {
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
/// `config`, which fixes the session, and its audio, in chunks Phonic takes.
#[derive(Debug)]
pub(super) struct Provider {
    /// The model routing chose, else the one at the client's `?model=`.
    model: Option<String>,
    /// Whether its client is Phonic's, whose voice names one of Phonic's.
    voiced: bool,
    /// The session as its `config` started it, once it did.
    applied: Applied,
    /// The session as the client asked it, which a later one is judged by.
    asked: Option<Session>,
    say: Say,
    /// The format of the audio it takes, as its `config` said.
    input: AudioFormat,
    convert: Convert,
}

impl Provider {
    pub(super) fn new(model: Option<String>, voiced: bool, applied: Applied, say: Say) -> Self {
        Self {
            model,
            voiced,
            applied,
            asked: None,
            say,
            input: DEFAULT_FORMAT,
            convert: Convert::default(),
        }
    }

    pub(super) fn write(&mut self, event: &Input, out: &mut Vec<Bytes>) -> Result<(), Fault> {
        let Some(event) = foreign::admit(NAME, event) else {
            return Ok(());
        };
        for v in self.encode(&event)? {
            out.push(json::frame(&v)?);
        }
        Ok(())
    }

    fn encode(&mut self, event: &Input) -> Result<Vec<Value>, Fault> {
        let started = self.asked.is_some();
        match event {
            Input::Session(s) if !started => Ok(vec![self.start(s)?]),
            Input::Session(s) => self.update(s).map(|()| Vec::new()),
            _ if !started => Err(Fault::internal("a Phonic session begins with its config")),
            Input::Audio { audio, native } => {
                let Some(data) = self.convert.to(audio, self.input) else {
                    return Err(
                        Unsupported::new(Why::Input, format!("audio in {}", audio.format))
                            .to(NAME)
                            .into(),
                    );
                };
                let chunks = data.chunks(chunk_bytes(self.input)).map(|chunk| {
                    let mut v = json!({"type": "audio_chunk", "audio": json::base64(chunk)});
                    merge(&mut v, own(native));
                    v
                });
                Ok(chunks.collect())
            }
            Input::Native(NativeValue { format, value }) if **format == *NAME => {
                Ok(vec![Value::clone(value)])
            }
            // Commits, messages, tools' answers, responses and cancels: it
            // has none of them as the kind has them.
            _ => Err(Unsupported::new(Why::Input, "this event").to(NAME).into()),
        }
    }

    /// The session's `config`: the model routing chose, else the
    /// handshake's, which routing judged, else its own; each audio format
    /// the client's if Phonic takes it, else its own, which the audio is
    /// converted to; a voice only from a Phonic client, as another
    /// format's names another vendor's. The provider's voice detection and
    /// transcription, which it always does, are dropped.
    fn start(&mut self, s: &Session) -> Result<Value, Fault> {
        self.asked = Some(s.clone());
        let mut s = s.clone();
        s.model = self.model.take().or(s.model);
        s.voice = s.voice.filter(|_| self.voiced);
        let unsupported = [
            (s.purpose == Some(Purpose::Transcription), "a transcription"),
            (s.output == Some(Modality::Text), "text output"),
            (s.turns == Some(Turns::Manual), "turns the client ends"),
            (s.tools.as_ref().is_some_and(|t| !t.is_empty()), "tools"),
            (s.max_output_tokens.is_some(), "a limit on output tokens"),
        ];
        if let Some((_, what)) = unsupported.iter().find(|(refused, _)| *refused) {
            return Err(Unsupported::new(Why::Input, *what).to(NAME).into());
        }
        s.input_audio = s.input_audio.filter(|f| session::takes(*f));
        s.output_audio = s.output_audio.filter(|f| session::takes(*f));
        self.input = s.input_audio.unwrap_or(DEFAULT_FORMAT);
        let mut v = session::encode(&s);
        merge(&mut v, own(&s.native));
        s.input_audio = Some(self.input);
        s.output_audio = Some(s.output_audio.unwrap_or(DEFAULT_FORMAT));
        *self.applied.lock() = Some(s);
        Ok(v)
    }

    /// A later session, which another format's client may send: one that
    /// changes nothing the client asked, acknowledged with the session as
    /// it was applied, as the rest is fixed at its start.
    fn update(&mut self, s: &Session) -> Result<(), Fault> {
        if let Some(field) = changes(self.asked.as_ref().unwrap_or(s), s) {
            let what = format!("changing `{field}` after its config");
            let refused = Unsupported::new(Why::Lasting, what).to(NAME);
            return Err(refused.fix("open a new session").into());
        }
        let applied = self.applied.lock().clone().unwrap_or_default();
        let _ = self.say.unbounded_send(Output::Updated(applied));
        Ok(())
    }
}

/// The bytes of [`CHUNK_MS`] of audio in `format`.
fn chunk_bytes(format: AudioFormat) -> usize {
    format.rate as usize * CHUNK_MS / 1000 * format.sample_bytes()
}
