//! What the model says: the Live API's server events as the kind's
//! [`Output`], read at the provider's edge and written at the client's.

use respan_gateway_core::{Fault, Native};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_realtime::{Audio, AudioFormat, Cause, Output};
use serde_json::{Value, json};

use super::input::DEFAULT_FORMAT;
use super::{Key, NAME, session};
use crate::audio::Convert;
use crate::foreign;
use crate::json::{self, at, merge, string, take, text, untranslatable_output};

/// A provider's frames as events.
#[derive(Debug)]
pub(super) struct Provider {
    /// The format of the model's audio, as the session says.
    output: AudioFormat,
    /// The seconds billed so far, as its usage last said.
    billed: f64,
}

impl Provider {
    pub(super) fn new() -> Self {
        Self {
            output: DEFAULT_FORMAT,
            billed: 0.0,
        }
    }

    pub(super) fn read(&mut self, raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
        let v: Value = serde_json::from_slice(&raw)
            .map_err(|_| Fault::upstream("a provider's frame is not a JSON event"))?;
        let mut rest = v.clone();
        let kind = take(&mut rest, &["type"], string, text).unwrap_or_default();
        if let Some(event) = self.decode(&kind, rest) {
            out.push(event);
            return Ok(());
        }
        // Its usage is the session's seconds so far: billed beyond the last.
        let seconds = at(&v, &["usage", "seconds"]).and_then(Value::as_f64);
        if let Some(seconds) = seconds.filter(|s| *s > self.billed) {
            out.push(Output::Seconds(seconds - self.billed));
            self.billed = seconds;
        }
        out.push(Output::Native(Native::json::<Key>(v)));
        Ok(())
    }

    /// A server event of type `kind`, whose other fields are `rest`; `None`
    /// for one the kind has no form for, or could not write back as it came.
    fn decode(&mut self, kind: &str, mut rest: Value) -> Option<Output> {
        let v = &mut rest;
        let delta = |v: &mut Value| take(v, &["delta"], string, text);
        Some(match kind {
            // Its start answers the client's, as an update does.
            "session.started" | "session.updated" => {
                let session = session::decode(rest);
                self.output = session.output_audio.unwrap_or(self.output);
                Output::Updated(session)
            }
            "session.output_audio.delta" => Output::Audio {
                audio: Audio {
                    data: json::take_audio(v, &["delta"])?,
                    format: self.output,
                },
                native: Native::json::<Key>(rest),
            },
            "session.input_transcript.delta" => Output::InputTranscript {
                text: delta(v)?,
                complete: false,
                native: Native::json::<Key>(rest),
            },
            "session.output_transcript.delta" => Output::Transcript {
                text: delta(v)?,
                native: Native::json::<Key>(rest),
            },
            "error" => Output::Error {
                cause: match v["error"]["type"].as_str() {
                    Some("invalid_request_error") => Cause::Invalid,
                    _ => Cause::Provider,
                },
                message: take(v, &["error", "message"], string, text)?,
                native: Native::json::<Key>(rest),
            },
            _ => return None,
        })
    }
}

/// The events written to the client. Its session's start answers the
/// client's, so the first session another format tells of is its start.
/// It has no turns, responses or token usage, nor a whole transcript of the
/// user's, whose deltas were written: what tells of them is not written.
#[derive(Debug)]
pub(super) struct Client {
    /// The format of the audio it takes, as its session says.
    output: AudioFormat,
    convert: Convert,
    /// Whether the session's start was written.
    started: bool,
}

impl Client {
    pub(super) fn new() -> Self {
        Self {
            output: DEFAULT_FORMAT,
            convert: Convert::default(),
            started: false,
        }
    }

    pub(super) fn write(&mut self, event: &Output, out: &mut Vec<Bytes>) -> Result<(), Fault> {
        let Some(event) = foreign::strip::<Key>(event) else {
            return Ok(());
        };
        if let Some(v) = self.encode(&event)? {
            out.push(json::frame(&v)?);
        }
        Ok(())
    }

    fn encode(&mut self, event: &Output) -> Result<Option<Value>, Fault> {
        let refuse = |what: &str| Err(untranslatable_output(NAME, what));
        let (mut v, native) = match event {
            Output::Opened(s) | Output::Updated(s) => {
                let format = s.output_audio.or(s.input_audio);
                self.output = format.filter(|f| session::takes(*f)).unwrap_or(self.output);
                let kind = match std::mem::replace(&mut self.started, true) {
                    false => "session.started",
                    true => "session.updated",
                };
                (
                    json!({"type": kind, "session": session::encode(s, format)}),
                    &s.native,
                )
            }
            Output::Audio { audio, native } => {
                let Some(data) = self.convert.to(audio, self.output) else {
                    return refuse("audio of this format");
                };
                let delta = json::base64(&data);
                (
                    json!({"type": "session.output_audio.delta", "delta": delta}),
                    native,
                )
            }
            Output::Transcript { text, native } => (
                json!({"type": "session.output_transcript.delta", "delta": text}),
                native,
            ),
            Output::InputTranscript {
                text,
                complete: false,
                native,
            } => (
                json!({"type": "session.input_transcript.delta", "delta": text}),
                native,
            ),
            Output::Error {
                cause,
                message,
                native,
            } => {
                let mut v = json!({"type": "error", "error": {"message": message}});
                // Its own error keeps its fields; another's cause is its type.
                if native.fields::<Key>().is_empty() {
                    let invalid = matches!(cause, Cause::Invalid | Cause::NothingToCancel);
                    v["error"]["type"] = if invalid {
                        "invalid_request_error"
                    } else {
                        "server_error"
                    }
                    .into();
                }
                (v, native)
            }
            Output::Text { .. } => return refuse("text"),
            Output::ToolCall { .. } => return refuse("a tool's call"),
            Output::Native(native) => return Ok(native.value::<Key>().cloned()),
            _ => return Ok(None),
        };
        merge(&mut v, native.fields::<Key>());
        Ok(Some(v))
    }
}
