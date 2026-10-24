//! Events as Responses, whole or streamed: the encoders.

use super::{
    ANNOTATION, RESPONSE_FIELDS, RESPONSES, USAGE, USAGE_FIELDS, response_body, status_of,
};
use crate::schema::{
    Gate, block_fields, check_usage, fits, merge_fields, merge_into, unix_now, unwritable_content,
};
use respan_gateway_conversation::{
    Assembled, AssembledItem, Block, Delta, Finish, Output, Start, Usage,
};
use respan_gateway_core::{Fault, NativeValue};
use respan_gateway_http::bytes::{Bytes, BytesMut};
use respan_gateway_http::{Encoder, raw::RawReply};
use respan_gateway_sse::event;
use serde_json::{Map, Value, json};

// ------------------------------------------------------------------- items

/// The id of the item numbered `index` of response `id`: `prefix_` and the
/// response's own.
fn item_id(prefix: &str, id: &str, index: usize) -> String {
    let tail = id.strip_prefix("resp_").unwrap_or(id);
    format!("{prefix}_{tail}_{index}")
}

fn output_text(text: &str, annotations: &[Value]) -> Value {
    json!({ "type": "output_text", "text": text, "annotations": annotations })
}

fn summary(text: &str) -> Value {
    match text.is_empty() {
        true => json!([]),
        false => json!([{ "type": "summary_text", "text": text }]),
    }
}

/// An item of block `kind`, saying `text` (a message's text, a call's
/// arguments or a reasoning's summary) as it is at `status`, with the
/// `fields` its provider gave it.
fn item_of(
    kind: &Block,
    id: &str,
    status: &str,
    text: &str,
    annotations: &[Value],
    fields: &Map<String, Value>,
) -> Result<Value, Fault> {
    let mut item = match kind {
        Block::Text => json!({
            "id": id, "type": "message", "status": status, "role": "assistant",
            "content": match status {
                "in_progress" => json!([]),
                _ => json!([output_text(text, annotations)]),
            },
        }),
        Block::ToolCall { id: call, name } => {
            let arguments = match (text.is_empty(), status) {
                (true, "completed") => "{}",
                _ => text,
            };
            json!({
                "id": id, "type": "function_call", "status": status,
                "arguments": arguments, "call_id": call, "name": name,
            })
        }
        Block::Reasoning => json!({
            "id": id, "type": "reasoning",
            "summary": match status {
                "in_progress" => json!([]),
                _ => summary(text),
            },
        }),
        _ => return Err(unwritable_content()),
    };
    let skip_status = status == "in_progress";
    let fields: Map<String, Value> = fields
        .iter()
        .filter(|(k, _)| !(skip_status && *k == "status"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    merge_fields(&mut item, &fields);
    Ok(item)
}

/// The fields of a native of `kind` that carries only `fields`.
fn fields_only<'a>(raw: &'a Value, kind: &str) -> Option<&'a Map<String, Value>> {
    (raw["type"] == kind).then_some(())?;
    raw["fields"].as_object()
}

// ------------------------------------------------------------------- whole

/// The events collected, as one response body.
pub(super) fn whole(whole: &Assembled) -> Result<Value, Fault> {
    check_usage(&whole.usage, USAGE)?;
    let mut fields: Vec<(u32, Map<String, Value>)> = Vec::new();
    let mut annotations: Vec<(u32, Value)> = Vec::new();
    let (mut usage_more, mut changed) = (Map::new(), Map::new());
    for item in whole.items() {
        let AssembledItem::Native(NativeValue { format, value: raw }) = item else {
            continue;
        };
        if **format != *RESPONSES {
            continue;
        }
        if let Some((block, more)) = block_fields(raw) {
            fields.push((block, more.clone()));
        } else if raw["type"] == ANNOTATION {
            let block = raw["block"].as_u64().and_then(|b| u32::try_from(b).ok());
            annotations.extend(block.map(|b| (b, raw["annotation"].clone())));
        } else if let Some(more) = fields_only(raw, USAGE_FIELDS) {
            merge_into(&mut usage_more, more);
        } else if let Some(more) = fields_only(raw, RESPONSE_FIELDS) {
            merge_into(&mut changed, more);
        }
    }
    let id = whole
        .start
        .as_ref()
        .map(|s| s.id.as_str())
        .unwrap_or_default();
    let mut output: Vec<Value> = Vec::new();
    for item in whole.items() {
        match item {
            AssembledItem::Block(part) => {
                let prefix = match part.kind {
                    Block::Text => "msg",
                    Block::ToolCall { .. } => "fc",
                    _ => "rs",
                };
                let mine = |(b, _): &&(u32, Map<String, Value>)| *b == part.block;
                let mut more = Map::new();
                fields
                    .iter()
                    .filter(mine)
                    .for_each(|(_, f)| merge_into(&mut more, f));
                let cites: Vec<Value> = annotations
                    .iter()
                    .filter(|(b, _)| *b == part.block)
                    .map(|(_, a)| a.clone())
                    .collect();
                let fallback = item_id(prefix, id, output.len());
                let item_id = more.get("id").and_then(Value::as_str).unwrap_or(&fallback);
                let item_id = item_id.to_owned();
                output.push(item_of(
                    &part.kind,
                    &item_id,
                    "completed",
                    &part.text,
                    &cites,
                    &more,
                )?);
            }
            AssembledItem::Native(NativeValue { format, value: raw }) if **format == *RESPONSES => {
                match raw["type"].as_str() {
                    Some("response.output_item.done") => output.push(raw["item"].clone()),
                    // The rest of an item is in its done event, or in a block.
                    Some(t) if t.starts_with("response.") => {}
                    Some(ANNOTATION | USAGE_FIELDS | RESPONSE_FIELDS) => {}
                    _ if block_fields(raw).is_some() => {}
                    _ => return Err(unwritable_content()),
                }
            }
            // Another format's: only a client of that format could read it.
            AssembledItem::Native(_) => {}
        }
    }
    let finish = whole.finish.as_ref().unwrap_or(&Finish::Stop);
    let (status, reason) = status_of(finish)?;
    let body = response_body(
        whole.start.as_ref(),
        unix_now(),
        status,
        reason.as_deref(),
        output,
        Some((&whole.usage, &usage_more)),
        &changed,
    );
    Ok(body.into())
}

// ------------------------------------------------------------------ stream

/// Writes events as Responses' stream: the response's `created` and
/// `in_progress` at the first need, each item's events, and at the end the
/// terminal event with the response whole.
///
/// Items are written one at a time, as Responses does: a block that opens
/// while another is open waits, with its deltas, until that one ends.
#[derive(Debug, Default)]
pub(super) struct Events {
    start: Option<Start>,
    began: bool,
    created: u64,
    seq: u64,
    usage: Usage,
    usage_more: Map<String, Value>,
    changed: Map<String, Value>,
    finish: Option<Finish>,
    blocks: Vec<Slot>,
    /// The block open on the wire.
    current: Option<u32>,
    /// The blocks waiting to open, in order.
    waiting: Vec<u32>,
    /// The `output_index` the next item takes.
    next: u32,
    /// The fields of blocks that have not opened, by block.
    fields: Vec<(u32, Map<String, Value>)>,
    /// The items that are done, by `output_index`.
    done: Vec<(u32, Value)>,
    /// The `output_index` a provider's item took here.
    foreign: Vec<(u64, u32)>,
    gate: Gate,
}

/// A block, by number, as the stream has it so far.
#[derive(Debug)]
struct Slot {
    block: u32,
    kind: Block,
    /// Its `output_index` once it is open on the wire.
    index: Option<u32>,
    item_id: String,
    fields: Map<String, Value>,
    /// What came for it while it waited.
    held: Option<Delta>,
    /// Its text, arguments or reasoning so far.
    text: String,
    annotations: Vec<Value>,
    /// Whether its reasoning summary part has begun.
    summary: bool,
    ended: bool,
}

impl Events {
    fn emit(&mut self, out: &mut BytesMut, mut v: Value) {
        v["sequence_number"] = self.seq.into();
        self.seq += 1;
        out.extend_from_slice(event(v["type"].as_str(), &v).as_bytes());
    }

    fn response(&self, status: &str, reason: Option<&str>, done: bool) -> Value {
        let output = match done {
            true => {
                let mut items = self.done.clone();
                items.sort_by_key(|(i, _)| *i);
                items.into_iter().map(|(_, item)| item).collect()
            }
            false => Vec::new(),
        };
        let usage = done.then_some((&self.usage, &self.usage_more));
        let now = if done { unix_now() } else { self.created };
        let mut body = response_body(
            self.start.as_ref(),
            self.created,
            status,
            reason,
            output,
            usage,
            &self.changed,
        );
        if done {
            body.insert("completed_at".into(), now.into());
        }
        body.into()
    }

    /// Writes `response.created` and `response.in_progress` once.
    fn begin(&mut self, out: &mut BytesMut) {
        if std::mem::replace(&mut self.began, true) {
            return;
        }
        self.created = unix_now();
        let response = self.response("in_progress", None, false);
        self.emit(
            out,
            json!({ "type": "response.created", "response": response }),
        );
        self.emit(
            out,
            json!({ "type": "response.in_progress", "response": response }),
        );
    }

    fn response_id(&self) -> &str {
        self.start
            .as_ref()
            .map(|s| s.id.as_str())
            .unwrap_or_default()
    }

    fn find(&mut self, block: u32) -> Result<&mut Slot, Fault> {
        let found = self.blocks.iter_mut().find(|b| b.block == block);
        found.ok_or_else(unwritable_content)
    }

    /// Opens a block on the wire, with what it held while it waited.
    fn open_wire(&mut self, block: u32, out: &mut BytesMut) -> Result<(), Fault> {
        let index = self.next;
        self.next += 1;
        let prefix = match self.find(block)?.kind {
            Block::Text => "msg",
            Block::ToolCall { .. } => "fc",
            _ => "rs",
        };
        let fallback = item_id(prefix, self.response_id(), index as usize);
        let slot = self.find(block)?;
        slot.index = Some(index);
        slot.item_id = match slot.fields.get("id").and_then(Value::as_str) {
            Some(id) => id.to_owned(),
            None => fallback,
        };
        let item = item_of(
            &slot.kind,
            &slot.item_id,
            "in_progress",
            "",
            &[],
            &slot.fields,
        )?;
        let (id, held, is_text) = (
            slot.item_id.clone(),
            slot.held.take(),
            slot.kind == Block::Text,
        );
        self.emit(
            out,
            json!({ "type": "response.output_item.added", "output_index": index, "item": item }),
        );
        if is_text {
            let part = output_text("", &[]);
            self.emit(
                out,
                json!({
                    "type": "response.content_part.added", "item_id": id,
                    "output_index": index, "content_index": 0, "part": part,
                }),
            );
        }
        if let Some(held) = held {
            self.write_delta(block, &held, out)?;
        }
        Ok(())
    }

    /// Writes a delta of a block that is open on the wire.
    fn write_delta(&mut self, block: u32, part: &Delta, out: &mut BytesMut) -> Result<(), Fault> {
        let text = part.as_str();
        if text.is_empty() {
            return Ok(());
        }
        let slot = self.find(block)?;
        let (id, index) = (slot.item_id.clone(), slot.index);
        slot.text.push_str(text);
        let begin_summary = matches!(part, Delta::Reasoning(_)) && !slot.summary;
        slot.summary |= begin_summary;
        let mut events = Vec::new();
        match part {
            Delta::Text(_) => events.push(json!({
                "type": "response.output_text.delta", "item_id": id, "output_index": index,
                "content_index": 0, "delta": text, "logprobs": [],
            })),
            Delta::Arguments(_) => events.push(json!({
                "type": "response.function_call_arguments.delta", "item_id": id,
                "output_index": index, "delta": text,
            })),
            Delta::Reasoning(_) => {
                if begin_summary {
                    events.push(json!({
                        "type": "response.reasoning_summary_part.added", "item_id": id,
                        "output_index": index, "summary_index": 0,
                        "part": { "type": "summary_text", "text": "" },
                    }));
                }
                events.push(json!({
                    "type": "response.reasoning_summary_text.delta", "item_id": id,
                    "output_index": index, "summary_index": 0, "delta": text,
                }));
            }
            _ => return Err(unwritable_content()),
        }
        events.into_iter().for_each(|e| self.emit(out, e));
        Ok(())
    }

    /// Ends a block that is open on the wire.
    fn close_wire(&mut self, block: u32, out: &mut BytesMut) -> Result<(), Fault> {
        let slot = self.find(block)?;
        let (id, index, text) = (slot.item_id.clone(), slot.index, slot.text.clone());
        let item = item_of(
            &slot.kind,
            &id,
            "completed",
            &text,
            &slot.annotations,
            &slot.fields,
        )?;
        let mut events = Vec::new();
        match &slot.kind {
            Block::Text => {
                let part = output_text(&text, &slot.annotations);
                events.push(json!({
                    "type": "response.output_text.done", "item_id": id, "output_index": index,
                    "content_index": 0, "text": text, "logprobs": [],
                }));
                events.push(json!({
                    "type": "response.content_part.done", "item_id": id,
                    "output_index": index, "content_index": 0, "part": part,
                }));
            }
            Block::ToolCall { name, .. } => {
                let arguments = if text.is_empty() { "{}" } else { text.as_str() };
                events.push(json!({
                    "type": "response.function_call_arguments.done", "item_id": id,
                    "name": name, "output_index": index, "arguments": arguments,
                }));
            }
            _ if slot.summary => {
                let part = json!({ "type": "summary_text", "text": text });
                events.push(json!({
                    "type": "response.reasoning_summary_text.done", "item_id": id,
                    "output_index": index, "summary_index": 0, "text": text,
                }));
                events.push(json!({
                    "type": "response.reasoning_summary_part.done", "item_id": id,
                    "output_index": index, "summary_index": 0, "part": part,
                }));
            }
            _ => {}
        }
        events.push(json!({
            "type": "response.output_item.done", "output_index": index, "item": item,
        }));
        events.into_iter().for_each(|e| self.emit(out, e));
        if let Some(index) = index {
            self.done.push((index, item));
        }
        Ok(())
    }

    /// Opens the waiting blocks, one after another, as far as each has ended.
    fn pump(&mut self, out: &mut BytesMut) -> Result<(), Fault> {
        while self.current.is_none() && !self.waiting.is_empty() {
            let block = self.waiting.remove(0);
            self.open_wire(block, out)?;
            match self.find(block)?.ended {
                true => self.close_wire(block, out)?,
                false => self.current = Some(block),
            }
        }
        Ok(())
    }

    fn open(&mut self, block: u32, kind: &Block, out: &mut BytesMut) -> Result<(), Fault> {
        if let Some(had) = self.blocks.iter().find(|b| b.block == block) {
            // A start repeated for a block that is open opens nothing more.
            return match had.kind == *kind && !had.ended {
                true => Ok(()),
                false => Err(unwritable_content()),
            };
        }
        self.begin(out);
        let fields = match self.fields.iter().position(|(b, _)| *b == block) {
            Some(at) => self.fields.remove(at).1,
            None => Map::new(),
        };
        self.blocks.push(Slot {
            block,
            kind: kind.clone(),
            index: None,
            item_id: String::new(),
            fields,
            held: None,
            text: String::new(),
            annotations: Vec::new(),
            summary: false,
            ended: false,
        });
        self.waiting.push(block);
        self.pump(out)
    }

    fn say(&mut self, block: u32, part: &Delta, out: &mut BytesMut) -> Result<(), Fault> {
        let slot = self.find(block)?;
        if slot.ended || !fits(part, &slot.kind) {
            return Err(unwritable_content());
        }
        match (slot.index, part.as_str().is_empty()) {
            (_, true) => Ok(()),
            (Some(_), false) => self.write_delta(block, part, out),
            // The block is not open on the wire yet: keep what it says.
            (None, false) => {
                match &mut slot.held {
                    Some(held) => held.as_mut_string().push_str(part.as_str()),
                    None => slot.held = Some(part.clone()),
                }
                Ok(())
            }
        }
    }

    fn close(&mut self, block: u32, out: &mut BytesMut) -> Result<(), Fault> {
        let slot = self.find(block)?;
        if std::mem::replace(&mut slot.ended, true) {
            return Ok(());
        }
        if slot.index.is_some() {
            self.close_wire(block, out)?;
            self.current = None;
            self.pump(out)?;
        }
        Ok(())
    }

    /// An annotation of a text block.
    fn annotate(
        &mut self,
        block: u32,
        annotation: &Value,
        out: &mut BytesMut,
    ) -> Result<(), Fault> {
        let slot = self.find(block)?;
        if slot.kind != Block::Text || slot.ended {
            return Err(unwritable_content());
        }
        let n = slot.annotations.len();
        slot.annotations.push(annotation.clone());
        if let Some(index) = slot.index {
            let id = slot.item_id.clone();
            self.emit(
                out,
                json!({
                    "type": "response.output_text.annotation.added", "item_id": id,
                    "output_index": index, "content_index": 0, "annotation_index": n,
                    "annotation": annotation,
                }),
            );
        }
        Ok(())
    }

    /// An event of an item the kind has no form for, as it came, under the
    /// `output_index` the item has here.
    fn foreign(&mut self, raw: &Value, out: &mut BytesMut) {
        self.begin(out);
        let mut v = raw.clone();
        let Some(theirs) = raw["output_index"].as_u64() else {
            return self.emit(out, v);
        };
        let known = self
            .foreign
            .iter()
            .find(|(t, _)| *t == theirs)
            .map(|(_, o)| *o);
        let kind = raw["type"].as_str().unwrap_or_default();
        let ours = match known {
            Some(ours) => ours,
            None => {
                let ours = self.next;
                self.next += 1;
                self.foreign.push((theirs, ours));
                // A done event of an item never added, as a body's: add it.
                if kind == "response.output_item.done" {
                    let added = json!({
                        "type": "response.output_item.added", "output_index": ours,
                        "item": raw["item"],
                    });
                    self.emit(out, added);
                }
                ours
            }
        };
        v["output_index"] = ours.into();
        if kind == "response.output_item.done" {
            self.done.push((ours, raw["item"].clone()));
        }
        self.emit(out, v);
    }

    /// Ends every block, then writes the terminal event.
    fn terminate(&mut self, out: &mut BytesMut) -> Result<(), Fault> {
        self.begin(out);
        let open: Vec<u32> = self.blocks.iter().map(|b| b.block).collect();
        for block in open {
            self.close(block, out)?;
        }
        let finish = self.finish.as_ref().unwrap_or(&Finish::Stop);
        let (status, reason) = status_of(finish)?;
        let response = self.response(status, reason.as_deref(), true);
        let kind = match status {
            "completed" => "response.completed",
            _ => "response.incomplete",
        };
        self.emit(out, json!({ "type": kind, "response": response }));
        Ok(())
    }

    fn write(&mut self, reply: &Output, out: &mut BytesMut) -> Result<(), Fault> {
        match reply {
            Output::Start(start) => _ = self.start.get_or_insert_with(|| start.clone()),
            Output::BlockStart { block, kind } => self.open(*block, kind, out)?,
            Output::Delta { block, delta } => self.say(*block, delta, out)?,
            Output::BlockEnd { block } => self.close(*block, out)?,
            Output::Usage(usage) => {
                self.usage += usage;
                check_usage(&self.usage, USAGE)?;
            }
            Output::Finish(finish) => {
                status_of(finish)?;
                self.finish = Some(finish.clone());
                self.begin(out);
            }
            Output::Native(NativeValue { format, value: raw }) if **format == *RESPONSES => {
                if let Some((block, fields)) = block_fields(raw) {
                    // Fields of a block that is open join its item: they are the
                    // done item's.
                    match self.blocks.iter_mut().find(|b| b.block == block) {
                        Some(slot) => merge_into(&mut slot.fields, fields),
                        None => self.fields.push((block, fields.clone())),
                    }
                } else if raw["type"] == ANNOTATION {
                    let block = raw["block"].as_u64().and_then(|b| u32::try_from(b).ok());
                    self.annotate(
                        block.ok_or_else(unwritable_content)?,
                        &raw["annotation"],
                        out,
                    )?;
                } else if let Some(fields) = fields_only(raw, USAGE_FIELDS) {
                    merge_into(&mut self.usage_more, fields);
                } else if let Some(fields) = fields_only(raw, RESPONSE_FIELDS) {
                    merge_into(&mut self.changed, fields);
                } else if let Value::String(data) = &**raw {
                    // An event that could not be read, as it came.
                    out.extend_from_slice(event(None, data).as_bytes());
                } else {
                    self.foreign(raw, out);
                }
            }
            // Another format's: only a client of that format could read it.
            Output::Native(_) => {}
            _ => return Err(unwritable_content()),
        }
        Ok(())
    }

    fn write_end(&mut self, end: &Result<(), Fault>, out: &mut BytesMut) -> Result<(), Fault> {
        let Err(f) = end else {
            return self.terminate(out);
        };
        // What the provider finished before it broke off is kept.
        if self.finish.is_some() {
            self.terminate(out)?;
        }
        let raw = f
            .get::<RawReply>()
            .map(|r| String::from_utf8_lossy(r.0.body()).into_owned());
        let said = raw.and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
        match said.filter(|v| v["type"].is_string()) {
            Some(v) => {
                let name = v["type"].as_str().map(str::to_owned);
                out.extend_from_slice(event(name.as_deref(), &v).as_bytes());
            }
            None => {
                let v = json!({
                    "type": "error",
                    "code": f.code(),
                    "message": f.message(),
                    "param": null,
                });
                self.emit(out, v);
            }
        }
        Ok(())
    }
}

impl Events {
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

impl Encoder<Output> for Events {
    fn encode(&mut self, reply: &Output, frames: &mut Vec<Bytes>) -> Result<(), Fault> {
        crate::schema::frame(frames, |out| self.encode_into(reply, out))
    }

    fn end(&mut self, end: Result<(), Fault>, frames: &mut Vec<Bytes>) -> Result<(), Fault> {
        crate::schema::frame(frames, |out| self.end_into(&end, out))
    }
}
