//! Preserve Soniox token revisions and expose stable source transcripts.

use respan_gateway_core::{ErrorCode, Fault, NativeFields};
use respan_gateway_http::{Decoder, bytes::Bytes, http::StatusCode};
use respan_gateway_http_faults::Status;
use respan_gateway_realtime::{Cause, Output};
use serde_json::{Value, json};

use super::NAME;
use crate::foreign;
use crate::json::{self, untranslatable_output};

#[derive(Debug, Default)]
pub(super) struct Provider {
    transcript: String,
    finished: bool,
    failure: Option<Fault>,
}

impl Provider {
    pub(super) fn read(&mut self, raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
        if self.finished {
            return Err(Fault::upstream(
                "Soniox sent an event after its terminal response",
            ));
        }
        let value: Value = serde_json::from_slice(&raw)
            .map_err(|_| Fault::upstream("Soniox response is not JSON"))?;
        let fields = value
            .as_object()
            .ok_or_else(|| Fault::upstream("Soniox response must be an object"))?;
        if let Some(code) = value.get("error_code") {
            let code = code
                .as_u64()
                .and_then(|code| u16::try_from(code).ok())
                .and_then(|code| StatusCode::from_u16(code).ok())
                .filter(|code| code.is_client_error() || code.is_server_error())
                .ok_or_else(|| Fault::upstream("Soniox error_code is not an error status code"))?;
            let message = value["error_message"]
                .as_str()
                .ok_or_else(|| Fault::upstream("Soniox error has no message"))?;
            let mut failure = Fault::upstream(message).with(Status(code));
            match value.get("error_type") {
                Some(Value::String(kind)) if !kind.is_empty() => {
                    failure = failure.with(ErrorCode(kind.clone().into()));
                }
                Some(_) => {
                    return Err(Fault::upstream(
                        "Soniox error_type is not a nonempty string",
                    ));
                }
                None => {}
            }
            out.push(Output::native(NAME, value.clone()));
            out.push(Output::Error {
                cause: if code.is_client_error() {
                    Cause::Invalid
                } else {
                    Cause::Provider
                },
                code: Some(code.as_u16().to_string()),
                message: message.to_owned(),
                native: Some(NativeFields::new(NAME, fields.clone())),
            });
            self.finished = true;
            self.failure = Some(failure);
            return Ok(());
        }
        let tokens = value["tokens"]
            .as_array()
            .ok_or_else(|| Fault::upstream("Soniox response has no tokens array"))?;
        if value
            .get("finished")
            .is_some_and(|value| !value.is_boolean())
        {
            return Err(Fault::upstream("Soniox finished is not a boolean"));
        }
        for token in tokens {
            if !token["text"].is_string() || !token["is_final"].is_boolean() {
                return Err(Fault::upstream("Soniox token requires text and is_final"));
            }
        }
        // Native clients need every revision, translated token and timing field.
        out.push(Output::native(NAME, value.clone()));
        let native = Some(NativeFields::new(NAME, fields.clone()));
        let mut delta = String::new();
        for token in tokens {
            if token["is_final"] != true
                || !matches!(
                    token.get("translation_status").and_then(Value::as_str),
                    None | Some("none" | "original")
                )
            {
                continue;
            }
            let text = token["text"].as_str().expect("validated text");
            if matches!(text, "<end>" | "<fin>") {
                self.delta(&mut delta, &native, out);
                self.complete(&native, out);
            } else {
                delta.push_str(text);
            }
        }
        self.delta(&mut delta, &native, out);
        if value["finished"] == true {
            self.complete(&native, out);
            self.finished = true;
        }
        Ok(())
    }

    fn delta(&mut self, delta: &mut String, native: &Option<NativeFields>, out: &mut Vec<Output>) {
        if delta.is_empty() {
            return;
        }
        self.transcript.push_str(delta);
        out.push(Output::InputTranscript {
            text: std::mem::take(delta),
            complete: false,
            native: native.clone(),
        });
    }

    fn complete(&mut self, native: &Option<NativeFields>, out: &mut Vec<Output>) {
        if self.transcript.is_empty() {
            return;
        }
        out.push(Output::InputTranscript {
            text: std::mem::take(&mut self.transcript),
            complete: true,
            native: native.clone(),
        });
    }
}

impl Decoder<Output> for Provider {
    fn decode(&mut self, raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
        self.read(raw, out)
    }
    fn end(&mut self, end: Result<(), Fault>, _out: &mut Vec<Output>) -> Result<(), Fault> {
        // Deliver the native error before failing the session's lifecycle.
        if let Some(failure) = &self.failure {
            return Err(failure.clone());
        }
        end?;
        if self.finished {
            Ok(())
        } else {
            Err(Fault::truncated())
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct Client {
    transcript: String,
}

impl Client {
    pub(super) fn write(&mut self, event: &Output, out: &mut Vec<Bytes>) -> Result<(), Fault> {
        let Some(event) = foreign::strip(NAME, event)? else {
            return Ok(());
        };
        let value = match &event {
            Output::Native(native) => Value::clone(&native.value),
            Output::InputTranscript {
                native: Some(native),
                ..
            }
            | Output::Error {
                native: Some(native),
                ..
            } if *native.format == *NAME => return Ok(()),
            Output::InputTranscript { text, complete, .. } => {
                let tokens = if *complete {
                    let suffix = text
                        .strip_prefix(&self.transcript)
                        .ok_or_else(|| untranslatable_output(NAME, "a revised final transcript"))?;
                    let mut tokens = Vec::new();
                    if !suffix.is_empty() {
                        tokens.push(json!({"text":suffix,"is_final":true}));
                    }
                    tokens.push(json!({"text":"<fin>","is_final":true}));
                    self.transcript.clear();
                    tokens
                } else {
                    self.transcript.push_str(text);
                    vec![json!({"text":text,"is_final":true})]
                };
                json!({"tokens":tokens})
            }
            Output::Error { code, message, .. } => {
                json!({"tokens":[],"error_code":code.as_deref().and_then(|code| code.parse::<u64>().ok()).unwrap_or(500),"error_message":message})
            }
            Output::Opened(_) | Output::Updated(_) | Output::Usage(_) => return Ok(()),
            _ => return Err(untranslatable_output(NAME, "this event")),
        };
        out.push(json::frame(&value)?);
        Ok(())
    }
}
