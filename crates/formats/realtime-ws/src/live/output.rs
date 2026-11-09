//! What the model says: the Live API's server events as the kind's
//! [`Output`], read at the provider's edge and written at the client's.

use respan_gateway_core::{Fault, NativeValue};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_realtime::{Audio, AudioFormat, Cause, Output};
use serde_json::{Value, json};

use super::input::DEFAULT_FORMAT;
use super::{NAME, session};
use crate::audio::Convert;
use crate::foreign;
use crate::json::{self, at, merge, own, string, take, text, untranslatable_output};

/// A provider's frames as events.
#[derive(Debug)]
pub(super) struct Provider {
    /// The format of the model's audio, as the session says.
    output: AudioFormat,
    /// The seconds billed so far, as its usage last said.
    billed: f64,
}

impl Default for Provider {
    fn default() -> Self {
        Self {
            output: DEFAULT_FORMAT,
            billed: 0.0,
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
            Some(kind) => {
                if let "session.usage.updated" | "session.closed" = kind.as_str() {
                    self.bill(&v, out);
                }
                out.push(Output::native(NAME, v));
            }
            None => out.push(Output::native(NAME, v)),
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
                native: json::native(NAME, rest),
            },
            "session.input_transcript.delta" => Output::InputTranscript {
                text: delta(v)?,
                complete: false,
                native: json::native(NAME, rest),
            },
            "session.output_transcript.delta" => Output::Transcript {
                text: delta(v)?,
                native: json::native(NAME, rest),
            },
            "error" => Output::Error {
                cause: match v["error"]["type"].as_str() {
                    Some("invalid_request_error") => Cause::Invalid,
                    _ => Cause::Provider,
                },
                message: take(v, &["error", "message"], string, text)?,
                code: take(v, &["error", "code"], string, text),
                native: json::native(NAME, rest),
            },
            _ => return None,
        };
        out.push(event);
        Some(())
    }

    /// The seconds `v` bills beyond those billed so far: its usage is the
    /// session's whole.
    fn bill(&mut self, v: &Value, out: &mut Vec<Output>) {
        let Some(seconds) = at(v, &["usage", "seconds"]).and_then(Value::as_f64) else {
            return;
        };
        let more = seconds - self.billed;
        if more > 0.0 {
            self.billed = seconds;
            out.push(Output::Seconds(more));
        }
    }
}

/// The events written to the client. The Live API has no responses or
/// turns: what tells of them is not written, nor are tokens used. Its
/// session's start answers the client's, so the first session another
/// format tells of is its start, with an id made up for it.
#[derive(Debug)]
pub(super) struct Client {
    /// The format of the audio it takes, as its session says.
    output: AudioFormat,
    /// The model's audio in that format.
    convert: Convert,
    /// Whether the session's start was written.
    started: bool,
    /// Whether part of the user's transcript was written since its last
    /// whole, which is then not written again.
    heard: bool,
    ids: u64,
}

impl Default for Client {
    fn default() -> Self {
        Self {
            output: DEFAULT_FORMAT,
            convert: Convert::default(),
            started: false,
            heard: false,
            ids: 0,
        }
    }
}

impl Client {
    pub(super) fn write(&mut self, event: &Output, out: &mut Vec<Bytes>) -> Result<(), Fault> {
        let Some(event) = foreign::strip(NAME, event) else {
            return Ok(());
        };
        if let Some(mut v) = self.encode(&event)? {
            // Its audio has no id, as it is many a second.
            let unnamed = matches!(event, Output::Audio { .. });
            if !owned(&event) && !unnamed && v.get("event_id").is_none() {
                v["event_id"] = self.id("event").into();
            }
            out.push(json::frame(&v)?);
        }
        Ok(())
    }

    fn encode(&mut self, event: &Output) -> Result<Option<Value>, Fault> {
        let refuse = |what: &str| untranslatable_output(NAME, what);
        let (mut v, native) = match event {
            Output::Opened(s) | Output::Updated(s) => {
                let format = s.output_audio.or(s.input_audio);
                self.output = format.filter(|f| session::takes(*f)).unwrap_or(self.output);
                let kind = match std::mem::replace(&mut self.started, true) {
                    false => "session.started",
                    true => "session.updated",
                };
                let mut v = json!({"type": kind, "session": session::encode(s, format)});
                if s.native.is_none() {
                    v["session"]["id"] = self.id("live").into();
                    v["session"]["status"] = "active".into();
                }
                (v, &s.native)
            }
            Output::Audio { audio, native } => {
                let data = self.convert.to(audio, self.output);
                let data = data.ok_or_else(|| refuse("audio of this format"))?;
                let v = json!({"type": "session.output_audio.delta", "delta": json::base64(&data)});
                (v, native)
            }
            Output::Transcript { text, native } => (
                json!({"type": "session.output_transcript.delta", "delta": text}),
                native,
            ),
            // It has no whole transcript: what was heard of it was written.
            Output::InputTranscript { complete: true, .. } if std::mem::take(&mut self.heard) => {
                return Ok(None);
            }
            Output::InputTranscript {
                text,
                complete,
                native,
            } => {
                self.heard = !complete;
                (
                    json!({"type": "session.input_transcript.delta", "delta": text}),
                    native,
                )
            }
            Output::Error {
                cause,
                code,
                message,
                native,
            } => {
                let mut v = json!({"type": "error", "error": {"message": message}});
                // Another format's code is its own: the Live API's type says
                // the cause.
                match (native, code) {
                    (Some(_), Some(code)) => v["error"]["code"] = code.as_str().into(),
                    (Some(_), None) => {}
                    (None, _) => {
                        v["error"]["type"] = match cause {
                            Cause::Invalid | Cause::NothingToCancel => "invalid_request_error",
                            _ => "server_error",
                        }
                        .into();
                    }
                }
                (v, native)
            }
            Output::SpeechStarted { .. }
            | Output::SpeechStopped { .. }
            | Output::ResponseStart { .. }
            | Output::ResponseDone { .. }
            | Output::Usage(_)
            | Output::Seconds(_) => return Ok(None),
            Output::Text { .. } => return Err(refuse("text")),
            Output::ToolCall { .. } => return Err(refuse("a tool's call")),
            Output::Native(NativeValue { format, value }) if **format == *NAME => {
                return Ok(Some(Value::clone(value)));
            }
            _ => return Err(refuse("another format's event")),
        };
        merge(&mut v, own(native));
        Ok(Some(v))
    }

    /// An id made up for what another format has none for: a prefix and a
    /// number, unique in its session.
    fn id(&mut self, prefix: &str) -> String {
        self.ids += 1;
        format!("{prefix}_gw{}", self.ids)
    }
}

/// Whether `event` is the Live API's, saying as it came what it says
/// beyond the kind's fields.
fn owned(event: &Output) -> bool {
    match event {
        Output::Opened(s) | Output::Updated(s) => s.native.is_some(),
        Output::Transcript { native, .. }
        | Output::InputTranscript { native, .. }
        | Output::Error { native, .. } => native.is_some(),
        _ => true,
    }
}
