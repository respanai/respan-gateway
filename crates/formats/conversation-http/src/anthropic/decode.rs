//! Anthropic Messages, streamed or whole, as events: the decoder.

use crate::schema::{
    Totals, ended, error_event, native_fields, says, says_error, unreadable_event,
    unsupported_fields,
};
use respan_gateway_conversation::{Block, Output, Start};
use respan_gateway_core::Fault;
use respan_gateway_http::bytes::Bytes;
use respan_gateway_sse::event_json;
use serde_json::{Map, Value, json};
use std::sync::Arc;

use super::{ANTHROPIC, extra_usage, finish_of, usage_of};

/// Decodes events and whole bodies of Anthropic Messages into events, as
/// the module says.
#[derive(Debug)]
pub struct AnthropicDecoder {
    name: Arc<str>,
    totals: Totals,
    done: bool,
    /// The usage fields with no unit that were last said, to say them once.
    usage_extra: Map<String, Value>,
    /// The indexes of blocks the kind has no form for: all their events are
    /// [`Output::Native`].
    others: Vec<u32>,
}

impl Default for AnthropicDecoder {
    fn default() -> Self {
        Self {
            name: Arc::from(ANTHROPIC),
            totals: Totals::default(),
            done: false,
            usage_extra: Map::new(),
            others: Vec::new(),
        }
    }
}

/// A block's event, as the stream says it, for what the kind has no form for.
pub(super) fn event_of(kind: &str, index: u32, fields: Value) -> Value {
    let mut v = json!({ "type": kind, "index": index });
    if let (Some(v), Some(fields)) = (v.as_object_mut(), fields.as_object()) {
        v.extend(fields.clone());
    }
    v
}

impl AnthropicDecoder {
    fn other(&self, v: Value, out: &mut Vec<Output>) {
        out.push(Output::native(self.name.clone(), v));
    }

    /// The events of a message: its start and usage so far.
    fn message(&mut self, m: &Value, out: &mut Vec<Output>) -> Result<(), Fault> {
        let known = [
            "id",
            "model",
            "type",
            "role",
            "content",
            "usage",
            "stop_reason",
            "stop_sequence",
        ];
        let mut start = Start::new(
            m["id"].as_str().unwrap_or_default(),
            m["model"].as_str().unwrap_or_default(),
        );
        start.native = native_fields(ANTHROPIC, unsupported_fields(m, &known, &[]));
        out.push(Output::Start(start));
        let usage = self.totals.increment(&usage_of(&m["usage"])?);
        if !usage.is_empty() {
            out.push(Output::Usage(usage));
        }
        self.usage_other(&m["usage"], out);
        Ok(())
    }

    /// The fields of the usage object `u` that have no unit, as one `Native`
    /// that carries them in a `message_delta`'s usage.
    fn usage_other(&mut self, u: &Value, out: &mut Vec<Output>) {
        let native = extra_usage(u);
        if !native.is_empty() && native != self.usage_extra {
            let event = json!({ "type": "message_delta", "delta": {}, "usage": native });
            self.other(event, out);
            self.usage_extra = native;
        }
    }

    /// The events that open block `block`, `b`, with its first content; a
    /// block of a kind the kind has no form for is one `Native` of `event`.
    fn open(&mut self, block: u32, b: &Value, event: Value, out: &mut Vec<Output>) {
        let text = || b["text"].as_str().filter(|t| !t.is_empty());
        match b["type"].as_str() {
            Some("text") => {
                self.fields(block, b, &["type", "text"], out);
                out.push(Output::block_start(block, Block::Text));
                out.extend(text().map(|t| Output::text(block, t)));
            }
            Some("tool_use") => {
                let call = b["id"].as_str().zip(b["name"].as_str());
                let input = b["input"].as_object().filter(|_| call.is_some());
                let (Some((id, name)), Some(input)) = (call, input) else {
                    return self.opaque(block, event, out);
                };
                let kind = Block::ToolCall {
                    id: id.into(),
                    name: name.into(),
                };
                self.fields(block, b, &["type", "id", "name", "input"], out);
                out.push(Output::block_start(block, kind));
                if !input.is_empty() {
                    out.push(Output::arguments(block, b["input"].to_string()));
                }
            }
            Some("thinking") => {
                self.fields(block, b, &["type", "thinking", "signature"], out);
                out.push(Output::block_start(block, Block::Reasoning));
                let thinking = b["thinking"].as_str().filter(|t| !t.is_empty());
                out.extend(thinking.map(|t| Output::reasoning(block, t)));
                // A stream's start has an empty one; its delta says it.
                if b["signature"].as_str().is_some_and(|s| !s.is_empty()) {
                    self.signature(block, &b["signature"], out);
                }
            }
            _ => self.opaque(block, event, out),
        }
    }

    /// The fields of block `b` the kind has no place for, beyond its `known`
    /// ones, as one `Native` before the block opens, which the encoder merges
    /// into the block's start.
    fn fields(&self, block: u32, b: &Value, known: &[&str], out: &mut Vec<Output>) {
        let rest = b.as_object().into_iter().flatten();
        let rest: Map<String, Value> = rest
            // Kept even when null or empty: an unknown field's presence is
            // part of what its format said.
            .filter(|(k, _)| !known.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if !rest.is_empty() {
            out.push(Output::block_fields(self.name.clone(), block, rest));
        }
    }

    /// Reasoning's `signature`, which its block carries back, as its fields.
    fn signature(&self, block: u32, signature: &Value, out: &mut Vec<Output>) {
        let fields = Map::from_iter([("signature".to_owned(), signature.clone())]);
        out.push(Output::block_fields(self.name.clone(), block, fields));
    }

    fn opaque(&mut self, block: u32, event: Value, out: &mut Vec<Output>) {
        self.others.push(block);
        self.other(event, out);
    }

    /// What a `message_delta` says beyond the finish and usage, as one
    /// `Native`, if it says anything: `delta` is its `delta` and `top` its own
    /// fields.
    fn stop_fields(&self, delta: &Value, top: &Value, out: &mut Vec<Output>) {
        let delta: Map<String, Value> = delta
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(k, v)| *k != "stop_reason" && says(v))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let top = top.as_object().into_iter().flatten();
        let top = top.filter(|(k, v)| !["type", "delta", "usage"].contains(&k.as_str()) && says(v));
        let mut event = Map::new();
        event.insert("type".into(), "message_delta".into());
        event.insert("delta".into(), delta.clone().into());
        event.extend(top.map(|(k, v)| (k.clone(), v.clone())));
        if !delta.is_empty() || event.len() > 2 {
            self.other(event.into(), out);
        }
    }

    fn finish(&self, reason: &Value, out: &mut Vec<Output>) {
        let finish = reason.as_str().map(finish_of);
        out.extend(finish.map(Output::finished));
    }
}

impl AnthropicDecoder {
    /// The events of one stream event, `raw`.
    pub(crate) fn event(&mut self, raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
        if self.done {
            return Ok(());
        }
        let Some(v) = event_json(&raw) else {
            unreadable_event(&self.name, &raw, out);
            return Ok(());
        };
        if says_error(&v) || v["type"] == "error" {
            return Err(error_event(&raw, &v));
        }
        let block = v["index"]
            .as_u64()
            .and_then(|i| u32::try_from(i).ok())
            .unwrap_or(0);
        let opaque = self.others.contains(&block);
        match v["type"].as_str() {
            Some("message_start") => self.message(&v["message"], out)?,
            Some("content_block_start") => self.open(block, &v["content_block"], v.clone(), out),
            Some("content_block_delta") if !opaque => {
                let delta = &v["delta"];
                let text = |key: &str| delta[key].as_str().unwrap_or_default();
                match delta["type"].as_str() {
                    Some("text_delta") => out.push(Output::text(block, text("text"))),
                    Some("input_json_delta") => {
                        out.push(Output::arguments(block, text("partial_json")))
                    }
                    Some("thinking_delta") => out.push(Output::reasoning(block, text("thinking"))),
                    Some("signature_delta") => self.signature(block, &delta["signature"], out),
                    _ => self.other(v, out),
                }
            }
            Some("content_block_stop") if !opaque => out.push(Output::block_end(block)),
            Some("message_delta") => {
                let usage = self.totals.increment(&usage_of(&v["usage"])?);
                if !usage.is_empty() {
                    out.push(Output::Usage(usage));
                }
                self.usage_other(&v["usage"], out);
                self.stop_fields(&v["delta"], &v, out);
                self.finish(&v["delta"]["stop_reason"], out);
            }
            Some("message_stop") => self.done = true,
            _ => self.other(v, out),
        }
        Ok(())
    }

    /// The events of a stream that ended well.
    fn end_stream(&mut self, _out: &mut [Output]) -> Result<(), Fault> {
        ended(self.done)
    }

    /// The events of one whole response body `v`.
    pub(crate) fn body(&mut self, v: &Value, out: &mut Vec<Output>) -> Result<(), Fault> {
        self.message(v, out)?;
        for (at, b) in v["content"].as_array().into_iter().flatten().enumerate() {
            let block = u32::try_from(at).unwrap_or(u32::MAX);
            let start = event_of("content_block_start", block, json!({ "content_block": b }));
            let before = out.len();
            self.open(block, b, start, out);
            let opaque = self.others.contains(&block);
            let stop = match opaque {
                true => event_of("content_block_stop", block, json!({})),
                false => {
                    out.push(Output::block_end(block));
                    continue;
                }
            };
            debug_assert!(out.len() > before);
            self.other(stop, out);
        }
        self.stop_fields(
            &json!({ "stop_sequence": v["stop_sequence"] }),
            &json!({}),
            out,
        );
        self.finish(&v["stop_reason"], out);
        self.done = true;
        Ok(())
    }
}

impl respan_gateway_http::Decoder<Output> for AnthropicDecoder {
    fn decode(&mut self, raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
        self.event(raw, out)
    }

    fn end(&mut self, end: Result<(), Fault>, out: &mut Vec<Output>) -> Result<(), Fault> {
        end?;
        self.end_stream(out)
    }
}
