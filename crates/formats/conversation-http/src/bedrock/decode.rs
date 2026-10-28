//! Converse, streamed or whole, as events: the decoder.

use crate::schema::{count, ended, says, unsupported_fields};
use respan_gateway_conversation::{Block, Output, Start, Usage};
use respan_gateway_core::{ErrorCode, Fault};
use respan_gateway_eventstream::{Message, Parser};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::StatusCode;
use respan_gateway_http_faults::Status;
use serde_json::{Map, Value, json};
use std::sync::Arc;

use super::{BEDROCK, finish_of};

/// Decodes the events of a ConverseStream, and the body of a Converse, into
/// events, as the module says.
#[derive(Debug)]
pub struct ConverseDecoder {
    name: Arc<str>,
    parser: Parser,
    started: bool,
    finished: bool,
    /// The blocks that are open: those a delta opened lazily, as Converse
    /// sends no start for text or reasoning.
    open: Vec<u32>,
    /// The blocks of a type the kind has no form for, whose events are all
    /// [`Output::Native`].
    others: Vec<u32>,
}

impl Default for ConverseDecoder {
    fn default() -> Self {
        Self {
            name: Arc::from(BEDROCK),
            parser: Parser::new(None),
            started: false,
            finished: false,
            open: Vec::new(),
            others: Vec::new(),
        }
    }
}

/// `event`, a stream event of `kind`, as the native value that carries it.
fn native_event(kind: &str, event: &Value) -> Value {
    let mut v = json!({ "type": kind });
    if let (Some(v), Some(fields)) = (v.as_object_mut(), event.as_object()) {
        v.extend(fields.clone());
    }
    v
}

/// The fault of a failure Bedrock sent inside a stream, by the type it
/// names: HTTP's status for it, where the API gives one.
fn failure(message: &Message) -> Fault {
    let kind = message
        .text(":exception-type")
        .or_else(|| message.text(":error-code"))
        .unwrap_or("error");
    let said = serde_json::from_slice::<Value>(&message.payload).ok();
    let text = said.as_ref().and_then(|v| v["message"].as_str());
    let text = text.or_else(|| message.text(":error-message"));
    let text = text.map_or_else(
        || String::from_utf8_lossy(&message.payload).into_owned(),
        str::to_owned,
    );
    let status = match kind {
        "validationException" => 400,
        "accessDeniedException" => 403,
        "resourceNotFoundException" => 404,
        "modelTimeoutException" => 408,
        "throttlingException" => 429,
        "internalServerException" => 500,
        "serviceUnavailableException" => 503,
        "modelStreamErrorException" => 424,
        _ => 502,
    };
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    Fault::upstream(text)
        .with(ErrorCode(kind.to_owned().into()))
        .with(Status(status))
}

/// Usage totals; [`Usage::input_tokens`] includes the cache reads and
/// writes Converse counts apart.
pub(super) fn usage_of(u: &Value) -> Result<Usage, Fault> {
    let n = |key: &str| count(u.get(key)).map(Option::unwrap_or_default);
    let (read, written) = (n("cacheReadInputTokens")?, n("cacheWriteInputTokens")?);
    Ok(Usage {
        input_tokens: n("inputTokens")?
            .saturating_add(read)
            .saturating_add(written),
        output_tokens: n("outputTokens")?,
        cached_input_tokens: read,
        cache_write_input_tokens: written,
        ..Usage::default()
    })
}

impl ConverseDecoder {
    fn other(&self, v: Value, out: &mut Vec<Output>) {
        out.push(Output::native(self.name.clone(), v));
    }

    /// The fields of a response beside its content, as an inert event that
    /// another format drops.
    fn fields(&self, kind: &str, v: &Value, known: &[&str], out: &mut Vec<Output>) {
        let rest = unsupported_fields(v, known, &[]);
        if !rest.is_empty() {
            self.other(json!({ "type": kind, "fields": rest }), out);
        }
    }

    fn start(&mut self, out: &mut Vec<Output>) {
        if !self.started {
            self.started = true;
            out.push(Output::Start(Start::new("", "")));
        }
    }

    /// Opens `block` as `kind` if it is not open.
    fn open(&mut self, block: u32, kind: Block, out: &mut Vec<Output>) {
        if !self.open.contains(&block) {
            self.open.push(block);
            out.push(Output::block_start(block, kind));
        }
    }

    fn close(&mut self, block: u32, out: &mut Vec<Output>) {
        if let Some(at) = self.open.iter().position(|b| *b == block) {
            self.open.remove(at);
            out.push(Output::block_end(block));
        }
    }

    /// Reasoning's `signature` or `redactedContent`, which its block carries
    /// back, as the block's fields.
    fn signed(&self, block: u32, fields: Map<String, Value>, out: &mut Vec<Output>) {
        out.push(Output::block_fields(self.name.clone(), block, fields));
    }

    /// The events of a reasoning block's `reasoningContent`, text or not.
    fn reasoning(&mut self, block: u32, r: &Value, out: &mut Vec<Output>) {
        self.open(block, Block::Reasoning, out);
        let text = &r["reasoningText"];
        let (words, signature) = match text.is_object() {
            true => (&text["text"], &text["signature"]),
            false => (&r["text"], &r["signature"]),
        };
        if let Some(words) = words.as_str().filter(|t| !t.is_empty()) {
            out.push(Output::reasoning(block, words));
        }
        for (key, value) in [
            ("signature", signature),
            ("redactedContent", &r["redactedContent"]),
        ] {
            if says(value) {
                let fields = Map::from_iter([(key.to_owned(), value.clone())]);
                self.signed(block, fields, out);
            }
        }
    }

    /// The events of one stream message.
    fn message(&mut self, message: &Message, out: &mut Vec<Output>) -> Result<(), Fault> {
        if matches!(message.text(":message-type"), Some("exception" | "error")) {
            return Err(failure(message));
        }
        let kind = message.text(":event-type").unwrap_or_default();
        let v = match message.payload.is_empty() {
            true => json!({}),
            false => serde_json::from_slice::<Value>(&message.payload)
                .map_err(|_| Fault::upstream("a stream event's payload is not JSON"))?,
        };
        self.event(kind, &v, out)
    }

    fn event(&mut self, kind: &str, v: &Value, out: &mut Vec<Output>) -> Result<(), Fault> {
        let block = v["contentBlockIndex"]
            .as_u64()
            .and_then(|i| u32::try_from(i).ok())
            .unwrap_or(0);
        let opaque = self.others.contains(&block);
        match kind {
            "messageStart" => self.start(out),
            "contentBlockStart" => {
                self.start(out);
                let tool = &v["start"]["toolUse"];
                match tool["toolUseId"].as_str().zip(tool["name"].as_str()) {
                    Some((id, name)) => {
                        let kind = Block::ToolCall {
                            id: id.into(),
                            name: name.into(),
                        };
                        self.open(block, kind, out);
                    }
                    None => {
                        self.others.push(block);
                        self.other(native_event(kind, v), out);
                    }
                }
            }
            "contentBlockDelta" if !opaque => {
                self.start(out);
                let delta = &v["delta"];
                if let Some(text) = delta["text"].as_str() {
                    self.open(block, Block::Text, out);
                    out.push(Output::text(block, text));
                } else if let Some(input) = delta["toolUse"]["input"].as_str() {
                    out.push(Output::arguments(block, input));
                } else if delta["reasoningContent"].is_object() {
                    self.reasoning(block, &delta["reasoningContent"], out);
                } else {
                    self.other(native_event(kind, v), out);
                }
            }
            "contentBlockStop" if !opaque => self.close(block, out),
            "messageStop" => {
                self.start(out);
                out.extend(
                    v["stopReason"]
                        .as_str()
                        .map(|r| Output::finished(finish_of(r))),
                );
                self.fields("stop_fields", v, &["stopReason"], out);
                self.finished = true;
            }
            "metadata" => {
                let usage = usage_of(&v["usage"])?;
                if !usage.is_empty() {
                    out.push(Output::Usage(usage));
                }
                self.fields("metadata_fields", v, &["usage"], out);
            }
            _ => self.other(native_event(kind, v), out),
        }
        Ok(())
    }

    /// The events of one whole response body `v`.
    pub(crate) fn body(&mut self, v: &Value, out: &mut Vec<Output>) -> Result<(), Fault> {
        self.start(out);
        let usage = usage_of(&v["usage"])?;
        if !usage.is_empty() {
            out.push(Output::Usage(usage));
        }
        let blocks = v["output"]["message"]["content"].as_array();
        for (at, b) in blocks.into_iter().flatten().enumerate() {
            let block = u32::try_from(at).unwrap_or(u32::MAX);
            if let Some(text) = b["text"].as_str() {
                if !text.is_empty() {
                    self.open(block, Block::Text, out);
                    out.push(Output::text(block, text));
                }
            } else if let Some((id, name)) = b["toolUse"]["toolUseId"]
                .as_str()
                .zip(b["toolUse"]["name"].as_str())
                .filter(|_| b["toolUse"]["input"].is_object())
            {
                let kind = Block::ToolCall {
                    id: id.into(),
                    name: name.into(),
                };
                self.open(block, kind, out);
                out.push(Output::arguments(block, b["toolUse"]["input"].to_string()));
            } else if b["reasoningContent"].is_object() {
                self.reasoning(block, &b["reasoningContent"], out);
            } else {
                self.other(
                    json!({ "type": "content_block", "index": block, "block": b }),
                    out,
                );
                continue;
            }
            self.close(block, out);
        }
        out.extend(
            v["stopReason"]
                .as_str()
                .map(|r| Output::finished(finish_of(r))),
        );
        let stop = json!({ "additionalModelResponseFields": v["additionalModelResponseFields"] });
        self.fields("stop_fields", &stop, &[], out);
        let known = [
            "output",
            "stopReason",
            "usage",
            "additionalModelResponseFields",
        ];
        self.fields("metadata_fields", v, &known, out);
        self.finished = true;
        Ok(())
    }
}

impl respan_gateway_http::Decoder<Output> for ConverseDecoder {
    fn decode(&mut self, raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
        for message in self.parser.push(raw)? {
            self.message(&message, out)?;
        }
        Ok(())
    }

    fn end(&mut self, end: Result<(), Fault>, _out: &mut Vec<Output>) -> Result<(), Fault> {
        end?;
        self.parser.finish()?;
        ended(self.finished)
    }

    fn limit(&mut self, bytes: Option<usize>) {
        self.parser.limit(bytes);
    }
}
