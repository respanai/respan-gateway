//! Gemini generateContent, streamed or whole, as events: the decoder.

use crate::schema::{
    Totals, error_event, native_fields, remainder, says, says_error, unreadable_event,
};
use respan_gateway_conversation::{Block, Finish, Output, Start};
use respan_gateway_core::Fault;
use respan_gateway_http::bytes::Bytes;
use respan_gateway_sse::event_json;
use serde_json::{Map, Value, json};
use std::sync::Arc;

use super::{BLOCKED, GEMINI, SYNTHETIC, extra_usage, finish_of, usage_of};

/// A block of text or reasoning that is open, and may take more.
#[derive(Debug, Clone, Copy)]
pub(super) struct Open {
    block: u32,
    reasoning: bool,
}

/// Decodes responses of Gemini, streamed or whole, into events, as the
/// module says.
#[derive(Debug)]
pub struct GeminiDecoder {
    name: Arc<str>,
    totals: Totals,
    started: bool,
    done: bool,
    /// The id of the response, which the ids of its calls are made from.
    id: String,
    next: u32,
    open: Option<Open>,
    /// Whether the response has made a tool call.
    called: bool,
    /// The response, candidate and usage fields last said, to say changes only.
    response: Map<String, Value>,
    candidate: Map<String, Value>,
    usage: Map<String, Value>,
}

impl Default for GeminiDecoder {
    fn default() -> Self {
        Self {
            name: Arc::from(GEMINI),
            totals: Totals::default(),
            started: false,
            done: false,
            id: String::new(),
            next: 0,
            open: None,
            called: false,
            response: Map::new(),
            candidate: Map::new(),
            usage: Map::new(),
        }
    }
}

impl GeminiDecoder {
    fn other(&self, v: Value, out: &mut Vec<Output>) {
        out.push(Output::native(self.name.clone(), v));
    }

    /// Says `fields` as a `kind` event, if they changed.
    fn changed(
        &self,
        kind: &str,
        fields: Map<String, Value>,
        last: &mut Map<String, Value>,
        out: &mut Vec<Output>,
    ) {
        if !fields.is_empty() && fields != *last {
            self.other(json!({ "type": kind, "fields": fields }), out);
            *last = fields;
        }
    }

    fn close(&mut self, out: &mut Vec<Output>) {
        out.extend(self.open.take().map(|o| Output::block_end(o.block)));
    }

    /// A new block: its number, with the fields beside its content said first.
    fn block(&mut self, fields: Map<String, Value>, out: &mut Vec<Output>) -> u32 {
        self.close(out);
        let block = self.next;
        self.next += 1;
        if !fields.is_empty() {
            out.push(Output::block_fields(GEMINI, block, fields));
        }
        block
    }

    /// The events of one part.
    fn part(&mut self, p: &Value, out: &mut Vec<Output>) {
        let payload = |k: &str| p.get(k).is_some();
        if let Some(text) = p["text"].as_str().filter(|_| payload("text")) {
            let reasoning = p["thought"] == true;
            let fields = remainder(p, &["text", "thought"]);
            let fields: Map<String, Value> =
                fields.into_iter().filter(|(_, v)| !v.is_null()).collect();
            let kind = || match reasoning {
                true => Block::Reasoning,
                false => Block::Text,
            };
            if !fields.is_empty() {
                let block = self.block(fields, out);
                out.push(Output::block_start(block, kind()));
                out.extend(self.delta(block, text, reasoning));
                out.push(Output::block_end(block));
            } else if !text.is_empty() {
                let block = match self.open.filter(|o| o.reasoning == reasoning) {
                    Some(open) => open.block,
                    None => {
                        let block = self.block(Map::new(), out);
                        out.push(Output::block_start(block, kind()));
                        self.open = Some(Open { block, reasoning });
                        block
                    }
                };
                out.extend(self.delta(block, text, reasoning));
            }
        } else if let Some(call) = p["functionCall"]
            .as_object()
            .filter(|_| payload("functionCall"))
        {
            let name = call.get("name").and_then(Value::as_str);
            let args = call.get("args").unwrap_or(&Value::Null);
            let clean = (args.is_object() || args.is_null())
                && call
                    .keys()
                    .all(|k| matches!(k.as_str(), "name" | "args" | "id"));
            let (Some(name), true) = (name, clean) else {
                return self.opaque(p, out);
            };
            let fields = remainder(p, &["functionCall"]);
            let block = self.block(fields, out);
            self.called = true;
            let id = match call.get("id").and_then(Value::as_str) {
                Some(id) => id.to_owned(),
                None if self.id.is_empty() => format!("{SYNTHETIC}{block}"),
                None => format!("{SYNTHETIC}{}_{block}", self.id),
            };
            out.push(Output::block_start(
                block,
                Block::ToolCall {
                    id,
                    name: name.into(),
                },
            ));
            let args = if args.is_null() {
                json!({})
            } else {
                args.clone()
            };
            out.push(Output::arguments(block, args.to_string()));
            out.push(Output::block_end(block));
        } else {
            self.opaque(p, out);
        }
    }

    /// A part the kind has no form for.
    fn opaque(&mut self, p: &Value, out: &mut Vec<Output>) {
        self.close(out);
        self.other(json!({ "type": "part", "part": p }), out);
    }

    fn delta(&self, block: u32, text: &str, reasoning: bool) -> Option<Output> {
        match (reasoning, text.is_empty()) {
            (_, true) => None,
            (true, _) => Some(Output::reasoning(block, text)),
            (false, _) => Some(Output::text(block, text)),
        }
    }

    /// The events of one response, whole or a stream's.
    fn response(&mut self, v: &Value, out: &mut Vec<Output>) -> Result<(), Fault> {
        let known = ["candidates", "usageMetadata", "modelVersion", "responseId"];
        let mut top = remainder(v, &known);
        top.retain(|_, v| says(v));
        if !self.started {
            self.started = true;
            self.id = v["responseId"].as_str().unwrap_or_default().to_owned();
            let model = v["modelVersion"].as_str().unwrap_or_default();
            let mut start = Start::new(self.id.clone(), model);
            start.native = native_fields(GEMINI, top.clone());
            self.response = top;
            out.push(Output::Start(start));
        } else {
            let mut last = std::mem::take(&mut self.response);
            self.changed("response_fields", top, &mut last, out);
            self.response = last;
        }
        let candidates = v["candidates"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        for (at, c) in candidates.iter().enumerate() {
            if at > 0 {
                self.other(json!({ "type": "candidate", "candidate": c }), out);
                continue;
            }
            for p in c["content"]["parts"].as_array().into_iter().flatten() {
                self.part(p, out);
            }
            let mut fields = remainder(c, &["content", "finishReason", "index"]);
            fields.retain(|_, v| says(v));
            let mut last = std::mem::take(&mut self.candidate);
            self.changed("candidate_fields", fields, &mut last, out);
            self.candidate = last;
            if let Some(reason) = c["finishReason"].as_str() {
                self.close(out);
                let finish = match (finish_of(reason), self.called) {
                    (Finish::Stop, true) => Finish::ToolUse,
                    (finish, _) => finish,
                };
                out.push(Output::finished(finish));
                self.done = true;
            }
        }
        if candidates.is_empty() && says(&v["promptFeedback"]["blockReason"]) {
            let filtered = Finish::native_or(GEMINI, BLOCKED, Finish::Filtered);
            out.push(Output::finished(filtered));
            self.done = true;
        }
        let usage = &v["usageMetadata"];
        if usage.is_object() {
            let step = self.totals.increment(&usage_of(usage)?);
            if !step.is_empty() {
                out.push(Output::Usage(step));
            }
            let mut last = std::mem::take(&mut self.usage);
            self.changed("usage_fields", extra_usage(usage), &mut last, out);
            self.usage = last;
        }
        Ok(())
    }
}

impl GeminiDecoder {
    /// The events of one stream event, `raw`.
    pub(crate) fn event(&mut self, raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
        let Some(v) = event_json(&raw) else {
            unreadable_event(&self.name, &raw, out);
            return Ok(());
        };
        if says_error(&v) {
            return Err(error_event(&raw, &v));
        }
        self.response(&v, out)
    }

    /// The events of a stream that ended well.
    fn end_stream(&mut self, out: &mut Vec<Output>) -> Result<(), Fault> {
        self.close(out);
        crate::schema::ended(self.done)
    }

    /// The events of one whole response body `v`.
    pub(crate) fn body(&mut self, v: &Value, out: &mut Vec<Output>) -> Result<(), Fault> {
        self.response(v, out)?;
        self.close(out);
        self.done = true;
        Ok(())
    }
}

impl respan_gateway_http::Decoder<Output> for GeminiDecoder {
    fn decode(&mut self, raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
        self.event(raw, out)
    }

    fn end(&mut self, end: Result<(), Fault>, out: &mut Vec<Output>) -> Result<(), Fault> {
        end?;
        self.end_stream(out)
    }
}
