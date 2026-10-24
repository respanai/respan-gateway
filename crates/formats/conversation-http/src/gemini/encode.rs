//! Events as Gemini generateContent, whole or streamed: the encoders.

use crate::schema::{
    Gate, Schema, arguments_object, block_fields, check_usage, merge_fields, merge_into,
    own_fields, unwritable_content,
};
use respan_gateway_conversation::{Block, Delta, Finish, Output, Start, Usage};
use respan_gateway_core::{Fault, NativeValue};
use respan_gateway_http::Encoder;
use respan_gateway_http::bytes::{Bytes, BytesMut};
use respan_gateway_sse::{event, event_data};
use serde_json::{Map, Value, json};

use super::request::is_synthetic;
use super::{BLOCKED, FINISH, GEMINI, GeminiGenerate, USAGE, usage_body};

/// Writes the SSE frames of `inner` as the elements of one JSON array, which
/// is how Gemini streams without `alt=sse`.
pub(super) struct JsonArray {
    inner: Box<dyn Encoder<Output>>,
    began: bool,
}

impl JsonArray {
    pub(super) fn new(inner: Box<dyn Encoder<Output>>) -> Self {
        Self {
            inner,
            began: false,
        }
    }

    /// The elements in `frames`, the SSE events `inner` wrote, each its
    /// `data` lines joined.
    fn elements(&mut self, frames: &[u8], out: &mut BytesMut) {
        let text = String::from_utf8_lossy(frames);
        for data in text.split("\n\n").filter_map(|f| event_data(f.as_bytes())) {
            out.extend_from_slice(if self.began { b",\n" } else { b"[" });
            self.began = true;
            out.extend_from_slice(data.as_bytes());
        }
    }
}

impl JsonArray {
    pub(crate) fn encode_into(&mut self, reply: &Output, out: &mut BytesMut) -> Result<(), Fault> {
        let mut made = Vec::new();
        self.inner.encode(reply, &mut made)?;
        self.elements(&made.concat(), out);
        Ok(())
    }

    pub(crate) fn end_into(
        &mut self,
        end: &Result<(), Fault>,
        out: &mut BytesMut,
    ) -> Result<(), Fault> {
        let mut made = Vec::new();
        let ended = self.inner.end(end.clone(), &mut made);
        self.elements(&made.concat(), out);
        ended?;
        out.extend_from_slice(if self.began { b"]" } else { b"[]" });
        Ok(())
    }
}

impl Encoder<Output> for JsonArray {
    fn encode(&mut self, reply: &Output, frames: &mut Vec<Bytes>) -> Result<(), Fault> {
        crate::schema::frame(frames, |out| self.encode_into(reply, out))
    }

    fn end(&mut self, end: Result<(), Fault>, frames: &mut Vec<Bytes>) -> Result<(), Fault> {
        crate::schema::frame(frames, |out| self.end_into(&end, out))
    }
}

/// The part of a block of `kind` holding `text`, with the call's `id` set
/// unless it is one made here.
pub(super) fn part_of(kind: &Block, text: &str) -> Result<Value, Fault> {
    Ok(match kind {
        Block::Text => json!({ "text": text }),
        Block::Reasoning => json!({ "text": text, "thought": true }),
        Block::ToolCall { id, name } => {
            let args = arguments_object(text).map_err(|_| unwritable_content())?;
            let mut call = json!({ "name": name, "args": args });
            if !is_synthetic(id) {
                call["id"] = id.as_str().into();
            }
            json!({ "functionCall": call })
        }
        _ => return Err(unwritable_content()),
    })
}

/// The fields a response keeps beside its parts, from the events that carry
/// them.
#[derive(Debug, Default)]
pub(super) struct ResponseFields {
    /// The first candidate's fields beside its content.
    candidate: Map<String, Value>,
    /// The response's own, beside its candidates and usage.
    top: Map<String, Value>,
    /// The usage fields with no unit.
    usage: Map<String, Value>,
    /// The other candidates, whole.
    more: Vec<Value>,
}

impl ResponseFields {
    /// Takes the fields `raw` carries, if it carries any.
    pub(super) fn add(&mut self, raw: &Value) -> bool {
        let extend = |into: &mut Map<String, Value>| {
            if let Some(fields) = raw["fields"].as_object() {
                merge_into(into, fields);
            }
        };
        match raw["type"].as_str() {
            Some("candidate_fields") => extend(&mut self.candidate),
            Some("response_fields") => extend(&mut self.top),
            Some("usage_fields") => extend(&mut self.usage),
            Some("candidate") => self.more.push(raw["candidate"].clone()),
            _ => return false,
        }
        true
    }

    /// The response of `parts` and the rest.
    pub(super) fn finish(
        self,
        start: Option<&Start>,
        parts: Vec<Value>,
        finish: &Finish,
        usage: &Usage,
    ) -> Result<Value, Fault> {
        let reason = reason_of(finish)?;
        let mut body = Map::new();
        body.extend(own_fields(start, GEMINI));
        let blocked = reason == BLOCKED;
        let mut candidates = Vec::new();
        if !blocked {
            let mut candidate = Map::new();
            if !parts.is_empty() {
                candidate.insert("content".into(), json!({ "role": "model", "parts": parts }));
            }
            candidate.insert("finishReason".into(), reason.into());
            candidate.insert("index".into(), 0.into());
            candidate.extend(self.candidate);
            candidates.push(Value::Object(candidate));
            candidates.extend(self.more);
            body.insert("candidates".into(), candidates.into());
        }
        if let Some(usage) = usage_body(usage, &self.usage) {
            body.insert("usageMetadata".into(), usage);
        }
        let (id, model) = start
            .map(|s| (s.id.as_str(), s.model.as_str()))
            .unwrap_or_default();
        if !model.is_empty() {
            body.insert("modelVersion".into(), model.into());
        }
        if !id.is_empty() {
            body.insert("responseId".into(), id.into());
        }
        body.extend(self.top);
        Ok(body.into())
    }
}

/// The reason to write for `finish`.
pub(super) fn reason_of(finish: &Finish) -> Result<&str, Fault> {
    match finish {
        Finish::Native { format, reason, .. } if **format == *GEMINI && reason == BLOCKED => {
            Ok(BLOCKED)
        }
        finish => FINISH.reason(finish),
    }
}

/// A block, by number, as the stream has it so far.
#[derive(Debug)]
pub(super) struct Slot {
    block: u32,
    kind: Block,
    /// A tool call's arguments, until it ends.
    arguments: String,
    /// The fields to write with its first part.
    fields: Option<Map<String, Value>>,
    ended: bool,
}

/// Writes events as Gemini's stream: a response for each delta of text, one
/// for each tool call once it ends, and the last with the finish and usage.
#[derive(Debug, Default)]
pub(super) struct EventStream {
    start: Option<Start>,
    began: bool,
    usage: Usage,
    finish: Option<Finish>,
    fields: ResponseFields,
    blocks: Vec<Slot>,
    /// The fields of blocks that have not opened, by block.
    pending: Vec<(u32, Map<String, Value>)>,
    gate: Gate,
}

impl EventStream {
    fn find(&mut self, block: u32) -> Result<&mut Slot, Fault> {
        let found = self.blocks.iter_mut().find(|b| b.block == block);
        found.ok_or_else(unwritable_content)
    }

    /// A response of these `parts`, with the finish and usage if `last`.
    fn chunk(&mut self, parts: Vec<Value>, last: Option<&Finish>) -> Result<Value, Fault> {
        let first = !std::mem::replace(&mut self.began, true);
        let mut v = Map::new();
        if first {
            v.extend(own_fields(self.start.as_ref(), GEMINI));
        }
        let mut candidate = Map::new();
        if !parts.is_empty() || last.is_none() {
            candidate.insert("content".into(), json!({ "role": "model", "parts": parts }));
        } else {
            candidate.insert("content".into(), json!({ "role": "model", "parts": [] }));
        }
        candidate.insert("index".into(), 0.into());
        let mut blocked = false;
        if let Some(finish) = last {
            let reason = reason_of(finish)?;
            blocked = reason == BLOCKED;
            candidate.insert("finishReason".into(), reason.into());
            candidate.extend(std::mem::take(&mut self.fields.candidate));
        }
        if !blocked {
            let mut candidates = vec![Value::Object(candidate)];
            if last.is_some() {
                candidates.append(&mut self.fields.more);
            }
            v.insert("candidates".into(), candidates.into());
        }
        if last.is_some() {
            if let Some(usage) = usage_body(&self.usage, &self.fields.usage) {
                v.insert("usageMetadata".into(), usage);
            }
            v.extend(std::mem::take(&mut self.fields.top));
        }
        if let Some(start) = &self.start {
            if !start.model.is_empty() {
                v.insert("modelVersion".into(), start.model.as_str().into());
            }
            if !start.id.is_empty() {
                v.insert("responseId".into(), start.id.as_str().into());
            }
        }
        Ok(Value::Object(v))
    }

    fn emit(&mut self, parts: Vec<Value>, out: &mut BytesMut) -> Result<(), Fault> {
        let v = self.chunk(parts, None)?;
        out.extend_from_slice(event(None, &v).as_bytes());
        Ok(())
    }

    fn open(&mut self, block: u32, kind: &Block) -> Result<(), Fault> {
        if let Some(had) = self.blocks.iter().find(|b| b.block == block) {
            // A start repeated for a block that is open opens nothing more.
            return match had.kind == *kind && !had.ended {
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
        let at = self.pending.iter().position(|(b, _)| *b == block);
        let fields = at.map(|i| self.pending.remove(i).1);
        self.blocks.push(Slot {
            block,
            kind: kind.clone(),
            arguments: String::new(),
            fields,
            ended: false,
        });
        Ok(())
    }

    fn say(&mut self, block: u32, delta: &Delta, out: &mut BytesMut) -> Result<(), Fault> {
        let slot = self.find(block)?;
        if slot.ended || !crate::schema::fits(delta, &slot.kind) {
            return Err(unwritable_content());
        }
        if delta.as_str().is_empty() {
            return Ok(());
        }
        if let Delta::Arguments(text) = delta {
            slot.arguments.push_str(text);
            return Ok(());
        }
        let mut part = part_of(&slot.kind, delta.as_str())?;
        if let Some(fields) = slot.fields.take() {
            merge_fields(&mut part, &fields);
        }
        self.emit(vec![part], out)
    }

    fn close(&mut self, block: u32, out: &mut BytesMut) -> Result<(), Fault> {
        let slot = self.find(block)?;
        if std::mem::replace(&mut slot.ended, true) {
            return Ok(());
        }
        let text = match slot.kind {
            Block::ToolCall { .. } => slot.arguments.as_str(),
            // Fields that no text carried go in a part of their own.
            _ => "",
        };
        let mut part = part_of(&slot.kind, text)?;
        let tool = matches!(slot.kind, Block::ToolCall { .. });
        match slot.fields.take() {
            Some(fields) => merge_fields(&mut part, &fields),
            None if !tool => return Ok(()),
            None => {}
        }
        self.emit(vec![part], out)
    }

    /// Ends every block, then writes the last response.
    fn finish(&mut self, out: &mut BytesMut) -> Result<(), Fault> {
        let open: Vec<u32> = self.blocks.iter().map(|b| b.block).collect();
        for block in open {
            self.close(block, out)?;
        }
        let finish = self.finish.clone().unwrap_or(Finish::Stop);
        let v = self.chunk(Vec::new(), Some(&finish))?;
        out.extend_from_slice(event(None, &v).as_bytes());
        Ok(())
    }
}

impl EventStream {
    pub(crate) fn encode_into(&mut self, reply: &Output, out: &mut BytesMut) -> Result<(), Fault> {
        let mut made = BytesMut::new();
        self.write(reply, &mut made)?;
        self.gate.pass(reply, &made, out);
        Ok(())
    }

    pub(crate) fn end_into(
        &mut self,
        end: &Result<(), Fault>,
        out: &mut BytesMut,
    ) -> Result<(), Fault> {
        if end.is_ok() {
            self.gate.release(out);
        }
        self.write_end(end, out)
    }
}

impl Encoder<Output> for EventStream {
    fn encode(&mut self, reply: &Output, frames: &mut Vec<Bytes>) -> Result<(), Fault> {
        crate::schema::frame(frames, |out| self.encode_into(reply, out))
    }

    fn end(&mut self, end: Result<(), Fault>, frames: &mut Vec<Bytes>) -> Result<(), Fault> {
        crate::schema::frame(frames, |out| self.end_into(&end, out))
    }
}

impl EventStream {
    fn write(&mut self, reply: &Output, out: &mut BytesMut) -> Result<(), Fault> {
        match reply {
            Output::Start(start) => _ = self.start.get_or_insert_with(|| start.clone()),
            Output::BlockStart { block, kind } => self.open(*block, kind)?,
            Output::Delta { block, delta } => self.say(*block, delta, out)?,
            Output::BlockEnd { block } => self.close(*block, out)?,
            Output::Usage(usage) => {
                self.usage += usage;
                check_usage(&self.usage, USAGE)?;
            }
            Output::Finish(finish) => {
                reason_of(finish)?;
                self.finish = Some(finish.clone());
            }
            Output::Native(NativeValue { format, value: raw }) if **format == *GEMINI => {
                match (block_fields(raw), raw["type"].as_str()) {
                    (Some((block, fields)), _) => {
                        match self.blocks.iter_mut().find(|b| b.block == block) {
                            Some(slot) => slot.fields = Some(fields.clone()),
                            None => self.pending.push((block, fields.clone())),
                        }
                    }
                    (None, Some("part")) => self.emit(vec![raw["part"].clone()], out)?,
                    _ if self.fields.add(raw) => {}
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
        if let Err(f) = end {
            // What the provider finished before it broke off is kept.
            if self.finish.is_some() {
                self.finish(out)?;
            }
            let body = match f
                .get::<respan_gateway_http::raw::RawReply>()
                .map(|r| r.0.body())
            {
                Some(raw) => String::from_utf8_lossy(raw).into_owned(),
                None => GeminiGenerate::error_body(f).to_string(),
            };
            out.extend_from_slice(event(None, &body).as_bytes());
            return Ok(());
        }
        self.finish(out)
    }
}
