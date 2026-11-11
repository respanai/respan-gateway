//! What the client says: Phonic's client messages as the kind's [`Input`],
//! read at the client's edge and written at the provider's.

use respan_gateway_core::{Fault, NativeValue};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_realtime::{
    Audio, AudioFormat, Encoding, Input, Modality, Output, Purpose, Session, ToolChoice,
    ToolResult, Turns,
};
use respan_gateway_ws::CloseCode;
use serde_json::{Value, json};

use super::{Applied, Asked, DEFAULT_FORMAT, NAME, session};
use crate::audio::Convert;
use crate::foreign;
use crate::json::{self, merge, own, string, take, text, untranslatable_input};
use crate::session::{Say, changes};

/// The longest audio chunk Phonic takes, in milliseconds.
const CHUNK_MS: usize = 40;

/// The silence written after a `mute`, in milliseconds: Phonic ends a
/// muted turn only once more audio reaches it.
const HUSH_MS: usize = 1_000;

/// A client's frames as events: its `config`, its audio and its tools'
/// answers; any other is native.
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
            Some("tool_call_output") => {
                let id = take(&mut rest, &["tool_call_id"], string, text);
                let output = take(&mut rest, &["output"], answered, |s| Some(answer(s)));
                id.zip(output).map(|(id, output)| {
                    let mut result = ToolResult::new(id, output);
                    result.native = json::native(NAME, rest);
                    Input::ToolResult(result)
                })
            }
            _ => None,
        };
        out.push(event.unwrap_or_else(|| Input::native(NAME, v)));
        Ok(())
    }
}

/// The client's events written to the provider: its first session as
/// `config`, which fixes the session, its audio, in chunks Phonic takes,
/// and its tools' answers.
///
/// With turns the client ends, the user's first audio of a turn unmutes
/// it and its commit mutes it, which makes Phonic answer. A response asked
/// for after a commit or a tool's answer, which Phonic answers itself, is
/// dropped.
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
    /// Whether the client ends the user's turns, as its `config` said.
    manual: bool,
    /// Whether the user's turn is open, with turns the client ends.
    listening: bool,
    /// Whether what the client last said makes Phonic answer.
    answered: bool,
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
            manual: false,
            listening: false,
            answered: false,
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
        let refuse = |what: &str| Err(untranslatable_input(NAME, what));
        let started = self.asked.is_some();
        match event {
            Input::Session(s) if !started => Ok(vec![self.start(s)?]),
            Input::Session(s) => self.update(s).map(|()| Vec::new()),
            _ if !started => Err(Fault::internal("a Phonic session begins with its config")),
            Input::Audio { audio, native } => {
                let Some(data) = self.convert.to(audio, self.input) else {
                    return refuse("audio of this format");
                };
                let mut frames = Vec::new();
                if self.manual && !std::mem::replace(&mut self.listening, true) {
                    frames.push(json!({"type": "unmute"}));
                }
                frames.extend(chunks(&data, self.input).map(|mut v| {
                    merge(&mut v, own(native));
                    v
                }));
                Ok(frames)
            }
            Input::Commit { .. } if self.manual => {
                (self.listening, self.answered) = (false, true);
                let hush = silence(self.input);
                let mute = std::iter::once(json!({"type": "mute"}));
                Ok(mute.chain(chunks(&hush, self.input)).collect())
            }
            Input::Commit { .. } => refuse("a commit under the provider's voice detection"),
            Input::ToolResult(result) => {
                self.answered = true;
                let mut v = json!({"type": "tool_call_output", "tool_call_id": result.id,
                    "output": answer(&result.output)});
                merge(&mut v, own(&result.native));
                Ok(vec![v])
            }
            Input::Respond { .. } if std::mem::take(&mut self.answered) => Ok(Vec::new()),
            Input::Native(NativeValue { format, value }) if **format == *NAME => {
                Ok(vec![Value::clone(value)])
            }
            // Messages, cancels and a response to nothing it answers: it
            // has none of them as the kind has them.
            _ => refuse("this event"),
        }
    }

    /// The session's `config`: the model routing chose, else the
    /// handshake's, which routing judged, else its own; each audio format
    /// the client's if Phonic takes it, else its own, which the audio is
    /// converted to; a voice only from a Phonic client, as another
    /// format's names another vendor's. Turns the client ends are its push
    /// to talk; the provider's voice detection and transcription, which it
    /// always does, are dropped.
    fn start(&mut self, s: &Session) -> Result<Value, Fault> {
        self.asked = Some(s.clone());
        let mut s = s.clone();
        s.model = self.model.take().or(s.model);
        s.voice = s.voice.filter(|_| self.voiced);
        let unsupported = [
            (s.purpose == Some(Purpose::Transcription), "a transcription"),
            (s.output == Some(Modality::Text), "text output"),
            (
                s.tool_choice
                    .as_ref()
                    .is_some_and(|c| *c != ToolChoice::Auto),
                "a tool choice but `auto`",
            ),
            (s.max_output_tokens.is_some(), "a limit on output tokens"),
        ];
        if let Some((_, what)) = unsupported.iter().find(|(refused, _)| *refused) {
            return Err(untranslatable_input(NAME, what));
        }
        self.manual = s.turns == Some(Turns::Manual);
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
            let fixed = format!(
                "Phonic fixes a session's configuration at its config: this update changes `{field}`"
            );
            return Err(Fault::bad_request(fixed).with(CloseCode(1008)));
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

/// `data`, audio in `format`, as chunks Phonic takes.
fn chunks(data: &[u8], format: AudioFormat) -> impl Iterator<Item = Value> {
    data.chunks(chunk_bytes(format))
        .map(|chunk| json!({"type": "audio_chunk", "audio": json::base64(chunk)}))
}

/// [`HUSH_MS`] of silence in `format`.
fn silence(format: AudioFormat) -> Vec<u8> {
    let sample = match format.encoding {
        Encoding::Mulaw => 0xFF,
        Encoding::Alaw => 0xD5,
        _ => 0,
    };
    vec![sample; format.rate as usize * HUSH_MS / 1000 * format.sample_bytes()]
}

/// What a tool answered, as the kind has it: text as it is, any other
/// value as JSON text.
fn answered(output: &Value) -> Option<String> {
    Some(match output {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    })
}

/// What a tool answered, as Phonic takes it: the value its JSON text
/// says, else the text.
fn answer(output: &str) -> Value {
    serde_json::from_str(output).unwrap_or_else(|_| output.into())
}
