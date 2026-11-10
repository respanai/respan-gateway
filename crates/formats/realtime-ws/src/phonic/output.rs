//! What the model says: Phonic's server messages as the kind's [`Output`],
//! read at the provider's edge and written at the client's.

use respan_gateway_core::ErrorCode;
use respan_gateway_http::http::StatusCode;
use std::time::Instant;

use respan_gateway_core::{Fault, NativeFields, NativeValue};
use respan_gateway_http::Decoder;
use respan_gateway_http::bytes::Bytes;
use respan_gateway_realtime::{Audio, Cause, Output, Status};
use serde_json::{Value, json};

use super::{Applied, Asked, Clock, DEFAULT_FORMAT, NAME};
use crate::audio::Convert;
use crate::foreign;
use crate::json::{self, merge, own, string, take, text};

/// A provider's frames as events, and the session's seconds.
pub(super) struct Provider {
    /// The session as its `config` started it.
    applied: Applied,
    clock: Clock,
    /// When the seconds so far were billed until, from the conversation's
    /// creation.
    billed: Option<Instant>,
    /// Whether a word of the user's turn was told.
    heard: bool,
}

impl Provider {
    pub(super) fn new(applied: Applied, clock: Clock) -> Self {
        Self {
            applied,
            clock,
            billed: None,
            heard: false,
        }
    }

    /// The seconds since the last billed, from the conversation's creation.
    fn bill(&mut self, out: &mut Vec<Output>) {
        let Some(since) = self.billed else {
            return;
        };
        let now = (self.clock)();
        let seconds = now.saturating_duration_since(since).as_secs_f64();
        if seconds > 0.0 {
            out.push(Output::Seconds(seconds));
            self.billed = Some(now);
        }
    }

    /// The events of a server message of type `kind`, whose other fields
    /// are `rest`; `false`, and none, for one the kind has no form for.
    fn events(&mut self, kind: &str, mut rest: Value, out: &mut Vec<Output>) -> bool {
        let v = &mut rest;
        let event = match kind {
            // Its readiness answers the client's `config`.
            "ready_to_start_conversation" => {
                let mut s = self.applied.lock().clone().unwrap_or_default();
                s.native = json::native(NAME, rest);
                Output::Updated(s)
            }
            // Each chunk of its audio carries the text it speaks.
            "audio_chunk" => {
                let Some(data) = json::take_audio(v, &["audio"]) else {
                    return false;
                };
                let said = take(v, &["text"], string, text).unwrap_or_default();
                if !said.is_empty() {
                    out.push(Output::Transcript {
                        text: said,
                        native: None,
                    });
                }
                let format = self.applied.lock().as_ref().and_then(|s| s.output_audio);
                Output::Audio {
                    audio: Audio {
                        data,
                        format: format.unwrap_or(DEFAULT_FORMAT),
                    },
                    native: json::native(NAME, rest),
                }
            }
            // The user's transcript, a word at a time, which a space
            // parts from the turn's last.
            "input_text" => {
                let Some(word) = take(v, &["text"], string, text) else {
                    return false;
                };
                let space = if std::mem::replace(&mut self.heard, true) {
                    " "
                } else {
                    ""
                };
                Output::InputTranscript {
                    text: format!("{space}{word}"),
                    complete: false,
                    native: json::native(NAME, rest),
                }
            }
            "user_started_speaking" => {
                self.heard = false;
                Output::SpeechStarted {
                    at_ms: None,
                    native: json::native(NAME, rest),
                }
            }
            "user_finished_speaking" => Output::SpeechStopped {
                at_ms: None,
                native: json::native(NAME, rest),
            },
            "assistant_started_speaking" => Output::ResponseStart {
                id: None,
                native: json::native(NAME, rest),
            },
            "assistant_finished_speaking" => Output::ResponseDone {
                status: Status::Completed,
                native: json::native(NAME, rest),
            },
            // What it said before the user spoke over it stays native.
            "interrupted_response" => Output::ResponseDone {
                status: Status::Interrupted,
                native: json::native(NAME, rest),
            },
            "error" => {
                let Some(message) = take(v, &["error", "message"], string, text) else {
                    return false;
                };
                Output::Error {
                    cause: Cause::Provider,
                    code: take(v, &["error", "code"], string, text),
                    message,
                    native: json::native(NAME, rest),
                }
            }
            _ => return false,
        };
        out.push(event);
        true
    }
}

impl Decoder<Output> for Provider {
    fn decode(&mut self, raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
        let v: Value = serde_json::from_slice(&raw)
            .map_err(|_| Fault::upstream("a provider's frame is not a JSON event"))?;
        let mut rest = v.clone();
        let kind = take(&mut rest, &["type"], string, text).unwrap_or_default();
        if kind == "conversation_created" {
            self.billed = Some((self.clock)());
        }
        // Not at each of its many audio chunks: the next other frame, or
        // its end, bills their seconds.
        if kind != "audio_chunk" {
            self.bill(out);
        }
        if !self.events(&kind, rest, out) {
            out.push(Output::native(NAME, v));
        }
        Ok(())
    }

    fn end(&mut self, end: Result<(), Fault>, out: &mut Vec<Output>) -> Result<(), Fault> {
        self.bill(out);
        end
    }
}

/// The events written to the client. Its readiness answers its `config`, so
/// the first session another format tells of is it, and the rest are not
/// written. The model's transcript goes with its next audio, or, told after
/// the last, with an empty chunk at its turn's end; the user's goes a word
/// at a time, as Phonic tells it, not whole; what Phonic has no message
/// for, such as usage, is not written.
#[derive(Debug)]
pub(super) struct Client {
    /// The format of the audio it takes, as its `config` says.
    asked: Asked,
    convert: Convert,
    /// Whether its readiness was written.
    ready: bool,
    /// The model's transcript not yet written with its audio.
    said: String,
    /// The user's word heard so far, and its part's native, written once
    /// the next word or another event begins.
    word: Option<(String, Option<NativeFields>)>,
}

impl Client {
    pub(super) fn new(asked: Asked) -> Self {
        Self {
            asked,
            convert: Convert::default(),
            ready: false,
            said: String::new(),
            word: None,
        }
    }

    pub(super) fn write(&mut self, event: &Output, out: &mut Vec<Bytes>) -> Result<(), Fault> {
        let Some(event) = foreign::strip(NAME, event) else {
            return Ok(());
        };
        if let Output::InputTranscript {
            text,
            complete: false,
            native,
        } = &event
        {
            return self.hear(text, native, out);
        }
        self.spoken(out)?;
        if matches!(event, Output::ResponseDone { .. }) {
            self.unsaid(out)?;
        }
        if let Some(v) = self.encode(&event)? {
            out.push(json::frame(&v)?);
        }
        Ok(())
    }

    /// A part of the user's transcript: another format's may be a part of
    /// a word, which a space begins.
    fn hear(
        &mut self,
        text: &str,
        native: &Option<NativeFields>,
        out: &mut Vec<Bytes>,
    ) -> Result<(), Fault> {
        if text.starts_with(char::is_whitespace) {
            self.spoken(out)?;
        }
        match &mut self.word {
            Some((word, _)) => word.push_str(text),
            None => self.word = Some((text.trim_start().to_owned(), native.clone())),
        }
        Ok(())
    }

    /// The user's word heard so far, as its `input_text`.
    fn spoken(&mut self, out: &mut Vec<Bytes>) -> Result<(), Fault> {
        let Some((word, native)) = self.word.take().filter(|(w, _)| !w.is_empty()) else {
            return Ok(());
        };
        let mut v = json!({"type": "input_text", "text": word});
        merge(&mut v, own(&native));
        out.push(json::frame(&v)?);
        Ok(())
    }

    /// The model's transcript told after its last audio, in an empty chunk.
    fn unsaid(&mut self, out: &mut Vec<Bytes>) -> Result<(), Fault> {
        if !self.said.is_empty() {
            let said = std::mem::take(&mut self.said);
            out.push(json::frame(
                &json!({"type": "audio_chunk", "audio": "", "text": said}),
            )?);
        }
        Ok(())
    }

    fn encode(&mut self, event: &Output) -> Result<Option<Value>, Fault> {
        let refuse = |what: &str| {
            Err(Fault::new(format!("{what} cannot be translated to {NAME}"))
                .with(ErrorCode("untranslatable".into()))
                .with(respan_gateway_http_faults::Status(StatusCode::BAD_GATEWAY)))
        };
        let typed = |kind: &str| json!({"type": kind});
        let (mut v, native) = match event {
            Output::Opened(s) | Output::Updated(s) => {
                if std::mem::replace(&mut self.ready, true) {
                    return Ok(None);
                }
                (typed("ready_to_start_conversation"), &s.native)
            }
            Output::Audio { audio, native } => {
                let asked = *self.asked.lock();
                let Some(data) = self.convert.to(audio, asked) else {
                    return refuse("audio of this format");
                };
                let said = std::mem::take(&mut self.said);
                let v = json!({"type": "audio_chunk", "audio": json::base64(&data), "text": said});
                (v, native)
            }
            Output::Transcript { text, .. } => {
                self.said.push_str(text);
                return Ok(None);
            }
            Output::SpeechStarted { native, .. } => (typed("user_started_speaking"), native),
            Output::SpeechStopped { native, .. } => (typed("user_finished_speaking"), native),
            Output::ResponseStart { native, .. } => (typed("assistant_started_speaking"), native),
            Output::ResponseDone {
                status: Status::Interrupted,
                native,
            } => (typed("interrupted_response"), native),
            Output::ResponseDone { native, .. } => (typed("assistant_finished_speaking"), native),
            Output::Error {
                code,
                message,
                native,
                ..
            } => {
                let mut v = json!({"type": "error", "error": {"message": message}});
                if let Some(code) = code {
                    v["error"]["code"] = code.as_str().into();
                }
                (v, native)
            }
            Output::Text { .. } => return refuse("text"),
            Output::ToolCall { .. } => return refuse("a tool's call"),
            Output::Native(NativeValue { format, value }) if **format == *NAME => {
                return Ok(Some(Value::clone(value)));
            }
            _ => return Ok(None),
        };
        merge(&mut v, own(native));
        Ok(Some(v))
    }
}
