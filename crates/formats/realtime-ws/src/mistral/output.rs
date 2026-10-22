//! What the model says: Mistral's server events as the kind's [`Output`],
//! read at the provider's edge, and the kind's written to a client as
//! Mistral's.

use respan_gateway_core::Fault;
use respan_gateway_http::bytes::Bytes;
use respan_gateway_realtime::{Cause, Output, Session, Usage};
use serde_json::{Value, json};

use super::{NAME, session};
use crate::foreign;
use crate::json::{self, at, count, merge, number, remove, string, take, text};

/// A provider's events as the kind's.
#[derive(Debug)]
pub(super) struct Provider;

impl Provider {
    pub(super) fn read(&mut self, raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
        let v: Value = serde_json::from_slice(&raw)
            .map_err(|_| Fault::upstream("a provider's event is not JSON"))?;
        match decode(&v) {
            Some(events) => out.extend(events),
            None => out.push(Output::native(NAME, v)),
        }
        Ok(())
    }
}

/// The events of `v`; `None` for one the kind has no form for.
fn decode(v: &Value) -> Option<Vec<Output>> {
    let mut rest = v.clone();
    let rest = &mut rest;
    let kind = take(rest, &["type"], string, text)?;
    let events = match kind.as_str() {
        "session.created" => vec![Output::Opened(session::decode(rest.clone()))],
        "session.updated" => vec![Output::Updated(session::decode(rest.clone()))],
        "transcription.text.delta" => {
            let text = take(rest, &["text"], string, text)?;
            vec![transcript(text, false, rest)]
        }
        "transcription.done" => {
            // Its usage first, which a client of Mistral's writes in it.
            let used = usage(rest);
            let text = take(rest, &["text"], string, text)?;
            let mut events: Vec<_> = used.map(Output::Usage).into_iter().collect();
            events.push(transcript(text, true, rest));
            events
        }
        "error" => {
            let code = take(rest, &["error", "code"], count, number);
            let message = match at(rest, &["error", "message"]) {
                Some(Value::String(message)) => message.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            };
            remove(rest, &["error", "message"]);
            let cause = match code {
                Some(400..500) => Cause::Invalid,
                _ => Cause::Provider,
            };
            vec![Output::Error {
                cause,
                code: code.map(|c| c.to_string()),
                message,
                native: json::native(NAME, std::mem::take(rest)),
            }]
        }
        _ => return None,
    };
    Some(events)
}

fn transcript(text: String, complete: bool, rest: &mut Value) -> Output {
    Output::InputTranscript {
        text,
        complete,
        native: json::native(NAME, std::mem::take(rest)),
    }
}

/// The tokens a session's end reports, taken from its `usage`, and its
/// total if it is theirs. Its input is the user's audio. The seconds of
/// audio it also reports, and its service tier, stay native.
fn usage(v: &mut Value) -> Option<Usage> {
    let input = take(v, &["usage", "prompt_tokens"], count, number);
    let output = take(v, &["usage", "completion_tokens"], count, number);
    let (input, output) = match (input, output) {
        (None, None) => return None,
        (input, output) => (input.unwrap_or(0), output.unwrap_or(0)),
    };
    let total = |t: &Value| count(t).filter(|t| *t == input + output);
    take(v, &["usage", "total_tokens"], total, number);
    let mut usage = Usage::default();
    usage.tokens.input_tokens = input;
    usage.tokens.output_tokens = output;
    usage.input_audio_tokens = input;
    Some(usage)
}

/// The provider's events written to a client as Mistral's: its session,
/// the user's transcript and errors. Mistral answers nothing, so what tells
/// of an answer, which a transcription has none of, is not written.
#[derive(Debug, Default)]
pub(super) struct Client {
    /// The usage the session reported since the last whole transcript,
    /// written in the next.
    usage: Option<Usage>,
}

impl Client {
    pub(super) fn write(&mut self, event: &Output, out: &mut Vec<Bytes>) -> Result<(), Fault> {
        let Some(event) = foreign::strip(NAME, event)? else {
            return Ok(());
        };
        if let Some(v) = self.encode(&event) {
            out.push(json::frame(&v)?);
        }
        Ok(())
    }

    fn encode(&mut self, event: &Output) -> Option<Value> {
        let (mut v, native) = match event {
            Output::Opened(s) => (opened("session.created", s), &s.native),
            Output::Updated(s) => (opened("session.updated", s), &s.native),
            Output::InputTranscript {
                text,
                complete: false,
                native,
            } => (
                json!({"type": "transcription.text.delta", "text": text}),
                native,
            ),
            Output::InputTranscript {
                text,
                complete: true,
                native,
            } => {
                let mut v = json!({"type": "transcription.done", "text": text});
                if let Some(u) = self.usage.take() {
                    let (input, output) = (u.tokens.input_tokens, u.tokens.output_tokens);
                    v["usage"] = json!({"prompt_tokens": input, "completion_tokens": output,
                        "total_tokens": input + output});
                }
                (v, native)
            }
            Output::Usage(u) => {
                *self.usage.get_or_insert_default() += u;
                return None;
            }
            Output::Error {
                code,
                message,
                native,
                ..
            } => {
                let mut v = json!({"type": "error", "error": {"message": message}});
                if let Some(code) = code {
                    v["error"]["code"] = code
                        .parse::<u64>()
                        .map_or(code.as_str().into(), Value::from);
                }
                (v, native)
            }
            Output::Native(native) => return Some(Value::clone(&native.value)),
            _ => return None,
        };
        merge(&mut v, native.as_ref().map(|n| &n.fields));
        Some(v)
    }
}

/// A session's event `kind`: its model and audio format, as Mistral says
/// them.
fn opened(kind: &str, s: &Session) -> Value {
    let mut v = json!({"type": kind, "session": {}});
    if let Some(model) = &s.model {
        v["session"]["model"] = model.as_str().into();
    }
    let format = s.input_audio.and_then(session::audio_format_json);
    if let Some(format) = format {
        v["session"]["audio_format"] = format;
    }
    v
}
