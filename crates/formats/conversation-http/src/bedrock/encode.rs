//! Events as Converse, whole or as ConverseStream's AWS event stream: the
//! encoders of a client's response.

use crate::schema::{Gate, UsageFields, arguments_object, check_usage, fits, unwritable_content};
use respan_gateway_conversation::{
    Assembled, AssembledItem, Block, Delta, Finish, Output, Usage, block_fields,
};
use respan_gateway_core::{Fault, FaultKind, NativeValue};
use respan_gateway_eventstream::{Header, Message};
use respan_gateway_http::Encoder;
use respan_gateway_http::bytes::{Bytes, BytesMut};
use respan_gateway_http_faults::Status;
use serde_json::{Map, Value, json};

use super::{BEDROCK, FINISH};

/// Converse counts cache writes, and reasoning only as output.
pub(super) const USAGE: UsageFields = UsageFields {
    cache_writes: true,
    reasoning: false,
};

/// `usage` as Converse's: its `inputTokens` apart from the cache reads and
/// writes, which it counts on their own, and its total of them all.
pub(super) fn usage_body(usage: &Usage) -> Value {
    let cache = usage.cached_input_tokens + usage.cache_write_input_tokens;
    let mut v = json!({
        "inputTokens": usage.input_tokens.saturating_sub(cache),
        "outputTokens": usage.output_tokens,
        "totalTokens": usage.input_tokens + usage.output_tokens,
    });
    if usage.cached_input_tokens > 0 {
        v["cacheReadInputTokens"] = usage.cached_input_tokens.into();
    }
    if usage.cache_write_input_tokens > 0 {
        v["cacheWriteInputTokens"] = usage.cache_write_input_tokens.into();
    }
    v
}

/// Why the model stopped, as Converse names it.
fn stop_reason(finish: Option<&Finish>) -> Result<&str, Fault> {
    FINISH.reason(finish.unwrap_or(&Finish::Stop))
}

/// A block's reasoning fields (`signature`, `redactedContent`) as a
/// `reasoningContent`, with its `text`.
fn reasoning_block(text: &str, fields: &Map<String, Value>) -> Value {
    match fields.get("redactedContent") {
        Some(redacted) => json!({ "reasoningContent": { "redactedContent": redacted } }),
        None => {
            let mut said = json!({ "text": text });
            if let Some(signature) = fields.get("signature") {
                said["signature"] = signature.clone();
            }
            json!({ "reasoningContent": { "reasoningText": said } })
        }
    }
}

/// The events of a response, collected, as one Converse body.
pub(super) fn whole(whole: &Assembled) -> Result<Value, Fault> {
    check_usage(&whole.usage, USAGE)?;
    let mut content: Vec<Value> = Vec::new();
    let mut at: Vec<(u32, usize)> = Vec::new();
    let mut fields: Vec<(u32, Map<String, Value>)> = Vec::new();
    let mut top = Map::new();
    for item in whole.items() {
        match item {
            AssembledItem::Block(part) => {
                let block = match &part.kind {
                    Block::Text if part.text.is_empty() => continue,
                    Block::Text => json!({ "text": part.text }),
                    Block::Reasoning => {
                        let at = fields.iter().position(|(b, _)| *b == part.block);
                        let said = at.map(|at| fields.remove(at).1).unwrap_or_default();
                        reasoning_block(&part.text, &said)
                    }
                    Block::ToolCall { id, name } => {
                        let input =
                            arguments_object(&part.text).map_err(|_| unwritable_content())?;
                        json!({ "toolUse": { "toolUseId": id, "name": name, "input": input } })
                    }
                    _ => return Err(unwritable_content()),
                };
                at.push((part.block, content.len()));
                content.push(block);
            }
            AssembledItem::Native(NativeValue { format, value: raw })
                if block_fields(raw).is_some() =>
            {
                let Some((block, more)) = block_fields(raw) else {
                    continue;
                };
                if **format != *BEDROCK {
                    continue;
                }
                match at.iter().find(|(b, _)| *b == block) {
                    // A signature after its reasoning block was placed.
                    Some((_, place)) => {
                        let text = content[*place]["reasoningContent"]["reasoningText"]["text"]
                            .as_str()
                            .unwrap_or_default()
                            .to_owned();
                        content[*place] = reasoning_block(&text, more);
                    }
                    None => fields.push((block, more.clone())),
                }
            }
            AssembledItem::Native(NativeValue { format, value: raw }) if **format == *BEDROCK => {
                match raw["type"].as_str() {
                    Some("metadata_fields" | "stop_fields") => {
                        let said = raw["fields"].as_object().into_iter().flatten();
                        top.extend(said.map(|(k, v)| (k.clone(), v.clone())));
                    }
                    Some("content_block") => content.push(raw["block"].clone()),
                    _ => return Err(unwritable_content()),
                }
            }
            // Another format's: only a client of that format could read it.
            AssembledItem::Native(_) => {}
        }
    }
    let mut v = Map::new();
    let message = json!({ "role": "assistant", "content": content });
    v.insert("output".into(), json!({ "message": message }));
    v.insert(
        "stopReason".into(),
        stop_reason(whole.finish.as_ref())?.into(),
    );
    v.insert("usage".into(), usage_body(&whole.usage));
    v.insert("metrics".into(), json!({ "latencyMs": 0 }));
    v.extend(top.into_iter().filter(|(_, v)| !v.is_null()));
    Ok(Value::Object(v))
}

/// An event of ConverseStream: its type and its JSON payload.
fn event(kind: &str, payload: &Value) -> Bytes {
    let text = |v: &str| Header::String(v.to_owned());
    Message {
        headers: vec![
            (":event-type".into(), text(kind)),
            (":content-type".into(), text("application/json")),
            (":message-type".into(), text("event")),
        ],
        payload: Bytes::from(payload.to_string()),
    }
    .encode()
}

/// The exception a stream that broke off ends with: its type by the
/// fault's status, as ConverseStream names its failures.
pub(super) fn exception(fault: &Fault) -> Bytes {
    let status = fault.get::<Status>().map(|s| s.0.as_u16());
    let kind = match (status, fault.kind()) {
        (Some(400), _) | (None, FaultKind::Client) => "validationException",
        (Some(429), _) => "throttlingException",
        (Some(503), _) => "serviceUnavailableException",
        (Some(408), _) => "modelTimeoutException",
        (Some(500), _) => "internalServerException",
        _ => "modelStreamErrorException",
    };
    let text = |v: &str| Header::String(v.to_owned());
    Message {
        headers: vec![
            (":exception-type".into(), text(kind)),
            (":content-type".into(), text("application/json")),
            (":message-type".into(), text("exception")),
        ],
        payload: Bytes::from(json!({ "message": fault.message() }).to_string()),
    }
    .encode()
}

/// Writes events as ConverseStream: `messageStart` at the first need, each
/// block's events by its index, and at the end `messageStop` and
/// `metadata`, or an exception.
#[derive(Debug, Default)]
pub(super) struct EventStream {
    began: bool,
    usage: Usage,
    finish: Option<Finish>,
    /// Each block, by number: its kind, its index, and whether it ended.
    blocks: Vec<(u32, Block, u32, bool)>,
    /// What the response says beside its content, for its last events.
    stop: Map<String, Value>,
    metadata: Map<String, Value>,
    gate: Gate,
}

impl EventStream {
    fn begin(&mut self, out: &mut BytesMut) {
        if !std::mem::replace(&mut self.began, true) {
            out.extend_from_slice(&event("messageStart", &json!({ "role": "assistant" })));
        }
    }

    fn find(&self, block: u32) -> Result<&(u32, Block, u32, bool), Fault> {
        let found = self.blocks.iter().find(|b| b.0 == block);
        found.ok_or_else(unwritable_content)
    }

    fn open(&mut self, block: u32, kind: &Block, out: &mut BytesMut) -> Result<(), Fault> {
        if let Some(had) = self.blocks.iter().find(|b| b.0 == block) {
            return match had.1 == *kind && !had.3 {
                true => Ok(()),
                false => Err(unwritable_content()),
            };
        }
        if !matches!(
            kind,
            Block::Text | Block::Reasoning | Block::ToolCall { .. }
        ) {
            return Err(unwritable_content());
        }
        self.begin(out);
        let index = u32::try_from(self.blocks.len()).unwrap_or(u32::MAX);
        self.blocks.push((block, kind.clone(), index, false));
        if let Block::ToolCall { id, name } = kind {
            let start = json!({ "toolUse": { "toolUseId": id, "name": name } });
            let v = json!({ "contentBlockIndex": index, "start": start });
            out.extend_from_slice(&event("contentBlockStart", &v));
        }
        Ok(())
    }

    fn say(&mut self, block: u32, part: &Delta, out: &mut BytesMut) -> Result<(), Fault> {
        let (_, kind, index, ended) = self.find(block)?;
        if *ended || !fits(part, kind) {
            return Err(unwritable_content());
        }
        let text = part.as_str();
        if text.is_empty() {
            return Ok(());
        }
        let delta = match part {
            Delta::Text(_) => json!({ "text": text }),
            Delta::Arguments(_) => json!({ "toolUse": { "input": text } }),
            Delta::Reasoning(_) => json!({ "reasoningContent": { "text": text } }),
            _ => return Err(unwritable_content()),
        };
        let v = json!({ "contentBlockIndex": index, "delta": delta });
        out.extend_from_slice(&event("contentBlockDelta", &v));
        Ok(())
    }

    /// A reasoning block's `signature` or `redactedContent`, as its delta.
    fn signed(&mut self, block: u32, fields: &Map<String, Value>, out: &mut BytesMut) {
        let Some((_, Block::Reasoning, index, _)) = self.blocks.iter().find(|b| b.0 == block)
        else {
            return;
        };
        for key in ["signature", "redactedContent"] {
            if let Some(value) = fields.get(key) {
                let delta = json!({ "reasoningContent": { key: value } });
                let v = json!({ "contentBlockIndex": index, "delta": delta });
                out.extend_from_slice(&event("contentBlockDelta", &v));
            }
        }
    }

    fn close(&mut self, block: u32, out: &mut BytesMut) -> Result<(), Fault> {
        let found = self.blocks.iter_mut().find(|b| b.0 == block);
        let (_, _, index, ended) = found.ok_or_else(unwritable_content)?;
        if !std::mem::replace(ended, true) {
            let v = json!({ "contentBlockIndex": *index });
            out.extend_from_slice(&event("contentBlockStop", &v));
        }
        Ok(())
    }

    /// Ends every block, then writes `messageStop` and `metadata`.
    fn finish(&mut self, out: &mut BytesMut) -> Result<(), Fault> {
        self.begin(out);
        let open: Vec<u32> = self.blocks.iter().map(|b| b.0).collect();
        for block in open {
            self.close(block, out)?;
        }
        let mut stop = self.stop.clone();
        let reason = stop_reason(self.finish.as_ref())?;
        stop.insert("stopReason".into(), reason.into());
        out.extend_from_slice(&event("messageStop", &Value::Object(stop)));
        let mut metadata = Map::new();
        metadata.insert("usage".into(), usage_body(&self.usage));
        metadata.insert("metrics".into(), json!({ "latencyMs": 0 }));
        metadata.extend(self.metadata.clone());
        out.extend_from_slice(&event("metadata", &Value::Object(metadata)));
        Ok(())
    }

    fn write(&mut self, reply: &Output, out: &mut BytesMut) -> Result<(), Fault> {
        match reply {
            Output::Start(_) => {}
            Output::BlockStart { block, kind } => self.open(*block, kind, out)?,
            Output::Delta { block, delta } => self.say(*block, delta, out)?,
            Output::BlockEnd { block } => self.close(*block, out)?,
            Output::Usage(usage) => {
                self.usage += usage;
                check_usage(&self.usage, USAGE)?;
            }
            Output::Finish(finish) => {
                FINISH.reason(finish)?;
                self.finish = Some(finish.clone());
                self.begin(out);
            }
            Output::Native(NativeValue { format, value: raw }) if block_fields(raw).is_some() => {
                if let (true, Some((block, fields))) = (**format == *BEDROCK, block_fields(raw)) {
                    self.signed(block, fields, out);
                }
            }
            Output::Native(NativeValue { format, value: raw }) if **format == *BEDROCK => {
                let said = raw["fields"].as_object().cloned().unwrap_or_default();
                match raw["type"].as_str() {
                    Some("stop_fields") => self.stop.extend(said),
                    Some("metadata_fields") => self.metadata.extend(said),
                    Some(kind) if raw.get("contentBlockIndex").is_some() => {
                        let mut v = Value::clone(raw);
                        if let Some(v) = v.as_object_mut() {
                            v.shift_remove("type");
                        }
                        self.begin(out);
                        out.extend_from_slice(&event(kind, &v));
                    }
                    _ => return Err(unwritable_content()),
                }
            }
            // Another format's: only a client of that format could read it.
            Output::Native(_) => {}
            _ => return Err(unwritable_content()),
        }
        Ok(())
    }

    fn write_end(&mut self, end: &Result<(), Fault>, out: &mut BytesMut) -> Result<(), Fault> {
        match end {
            Ok(()) => self.finish(out),
            Err(fault) => {
                out.extend_from_slice(&exception(fault));
                Ok(())
            }
        }
    }
}

impl Encoder<Output> for EventStream {
    fn encode(&mut self, reply: &Output, frames: &mut Vec<Bytes>) -> Result<(), Fault> {
        crate::schema::frame(frames, |out| {
            let mut made = BytesMut::new();
            self.write(reply, &mut made)?;
            self.gate.pass(reply, &made, out);
            Ok(())
        })
    }

    fn end(&mut self, end: Result<(), Fault>, frames: &mut Vec<Bytes>) -> Result<(), Fault> {
        crate::schema::frame(frames, |out| {
            if end.is_ok() {
                self.gate.release(out);
            }
            self.write_end(&end, out)
        })
    }
}
