//! Events as Anthropic Messages, whole or streamed: the encoders.

use crate::schema::{
    Gate, Schema, check_usage, fits, merge_fields, own_fields, unwritable_content,
};
use respan_gateway_conversation::{Block, Delta, Finish, Output, Start, Usage};
use respan_gateway_core::{Fault, NativeValue};
use respan_gateway_http::Encoder;
use respan_gateway_http::bytes::{Bytes, BytesMut};
use respan_gateway_sse::event;
use serde_json::{Map, Value, json};

use super::{ANTHROPIC, AnthropicMessages, FINISH, StopFields, USAGE, usage_body};

/// Writes events as Anthropic's stream: `message_start` at the first need,
/// each block's events, and at the end `message_delta` and `message_stop`.
/// Blocks are written one at a time: one that opens while another is open
/// waits, and goes out when the open one ends.
#[derive(Debug, Default)]
pub(super) struct EventStream {
    start: Option<Start>,
    began: bool,
    usage: Usage,
    finish: Option<Finish>,
    stop: StopFields,
    blocks: Vec<Slot>,
    /// The block open on the wire.
    current: Option<u32>,
    /// The blocks waiting to open, in order.
    waiting: Vec<u32>,
    /// The index the next block takes.
    next: u32,
    /// The fields of blocks that have not opened, by block.
    fields: Vec<(u32, Map<String, Value>)>,
    gate: Gate,
}

/// A block, by number, as the stream has it so far.
#[derive(Debug)]
pub(super) struct Slot {
    block: u32,
    kind: Block,
    /// Its index once it is open on the wire.
    index: Option<u32>,
    /// What came for it while it waited.
    held: Option<Delta>,
    ended: bool,
}

impl EventStream {
    /// Writes `message_start` once.
    fn begin(&mut self, out: &mut BytesMut) {
        if std::mem::replace(&mut self.began, true) {
            return;
        }
        let (id, model) = self
            .start
            .as_ref()
            .map(|s| (s.id.as_str(), s.model.as_str()))
            .unwrap_or_default();
        let mut message = Map::new();
        message.insert("id".into(), id.into());
        message.insert("type".into(), "message".into());
        message.insert("role".into(), "assistant".into());
        message.insert("model".into(), model.into());
        message.extend(own_fields(self.start.as_ref(), ANTHROPIC));
        message.insert("content".into(), json!([]));
        message.insert("stop_reason".into(), Value::Null);
        message.insert("stop_sequence".into(), Value::Null);
        // What is known so far.
        message.insert("usage".into(), usage_body(&self.usage, &Map::new()));
        emit(out, json!({ "type": "message_start", "message": message }));
    }

    fn find(&mut self, block: u32) -> Result<&mut Slot, Fault> {
        let found = self.blocks.iter_mut().find(|b| b.block == block);
        found.ok_or_else(unwritable_content)
    }

    /// Opens a block on the wire, with what it held while it waited.
    fn open_wire(&mut self, block: u32, out: &mut BytesMut) -> Result<(), Fault> {
        let index = self.next;
        self.next += 1;
        let wb = self.find(block)?;
        wb.index = Some(index);
        let content = match &wb.kind {
            Block::Text => json!({ "type": "text", "text": "" }),
            Block::ToolCall { id, name } => {
                json!({ "type": "tool_use", "id": id, "name": name, "input": {} })
            }
            Block::Reasoning => json!({ "type": "thinking", "thinking": "" }),
            _ => return Err(unwritable_content()),
        };
        let held = wb.held.take();
        let mut content = content;
        if let Some(at) = self.fields.iter().position(|(b, _)| *b == block) {
            merge_fields(&mut content, &self.fields.remove(at).1);
        }
        let start =
            json!({ "type": "content_block_start", "index": index, "content_block": content });
        emit(out, start);
        if let Some(held) = held {
            delta(index, &held, out);
        }
        Ok(())
    }

    /// Opens the waiting blocks, one after another, as far as each has ended.
    fn pump(&mut self, out: &mut BytesMut) -> Result<(), Fault> {
        while self.current.is_none() && !self.waiting.is_empty() {
            let block = self.waiting.remove(0);
            self.open_wire(block, out)?;
            let wb = self.find(block)?;
            match (wb.ended, wb.index) {
                (true, Some(index)) => {
                    emit(out, json!({ "type": "content_block_stop", "index": index }))
                }
                _ => self.current = Some(block),
            }
        }
        Ok(())
    }

    /// The signature of thinking `block`: a `signature_delta` once it is on
    /// the wire, else part of its start.
    fn signature(&mut self, block: u32, data: &str, out: &mut BytesMut) {
        // WARN: Anthropic signs only thinking; another block's signature is dropped here, silently.
        let thinking = |b: &&Slot| b.block == block && b.kind == Block::Reasoning;
        let Some(slot) = self.blocks.iter().find(thinking) else {
            return;
        };
        match slot.index {
            Some(index) => {
                let delta = json!({ "type": "signature_delta", "signature": data });
                let event =
                    json!({ "type": "content_block_delta", "index": index, "delta": delta });
                emit(out, event);
            }
            None => {
                let signed = Map::from_iter([("signature".to_owned(), data.into())]);
                self.fields.push((block, signed));
            }
        }
    }

    /// Reasoning withheld, as a `redacted_thinking` block of its own: only a
    /// provider that says its blocks one at a time withholds it, so no other
    /// block is open on the wire.
    fn withheld(&mut self, data: &str, out: &mut BytesMut) {
        self.begin(out);
        let index = self.next;
        self.next += 1;
        let block = json!({ "type": "redacted_thinking", "data": data });
        emit(
            out,
            json!({ "type": "content_block_start", "index": index, "content_block": block }),
        );
        emit(out, json!({ "type": "content_block_stop", "index": index }));
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
        self.blocks.push(Slot {
            block,
            kind: kind.clone(),
            index: None,
            held: None,
            ended: false,
        });
        self.waiting.push(block);
        self.pump(out)
    }

    fn say(&mut self, block: u32, part: &Delta, out: &mut BytesMut) -> Result<(), Fault> {
        let wb = self.find(block)?;
        if wb.ended || !fits(part, &wb.kind) {
            return Err(unwritable_content());
        }
        match (wb.index, part.as_str().is_empty()) {
            (_, true) => {}
            (Some(index), false) => delta(index, part, out),
            // The block is not open on the wire yet: keep what it says.
            (None, false) => match &mut wb.held {
                Some(held) => held.as_mut_string().push_str(part.as_str()),
                None => wb.held = Some(part.clone()),
            },
        }
        Ok(())
    }

    fn close(&mut self, block: u32, out: &mut BytesMut) -> Result<(), Fault> {
        let wb = self.find(block)?;
        if std::mem::replace(&mut wb.ended, true) {
            return Ok(());
        }
        if let Some(index) = wb.index {
            emit(out, json!({ "type": "content_block_stop", "index": index }));
            self.current = None;
            self.pump(out)?;
        }
        Ok(())
    }

    /// Ends every block, then writes the terminal `message_delta`.
    fn finish(&mut self, out: &mut BytesMut) -> Result<(), Fault> {
        self.begin(out);
        let open: Vec<u32> = self.blocks.iter().map(|b| b.block).collect();
        for block in open {
            self.close(block, out)?;
        }
        let finish = self.finish.as_ref().unwrap_or(&Finish::Stop);
        let mut delta = Map::new();
        delta.insert("stop_reason".into(), self.stop.reason(finish)?.into());
        delta.insert("stop_sequence".into(), Value::Null);
        delta.extend(self.stop.delta.clone());
        let mut v = Map::new();
        v.insert("type".into(), "message_delta".into());
        v.insert("delta".into(), delta.into());
        v.insert("usage".into(), usage_body(&self.usage, &self.stop.usage));
        v.extend(self.stop.top.clone());
        emit(out, v.into());
        Ok(())
    }
}

pub(super) fn delta(index: u32, part: &Delta, out: &mut BytesMut) {
    let text = part.as_str();
    let delta = match part {
        Delta::Text(_) => json!({ "type": "text_delta", "text": text }),
        Delta::Arguments(_) => json!({ "type": "input_json_delta", "partial_json": text }),
        Delta::Reasoning(_) => json!({ "type": "thinking_delta", "thinking": text }),
        _ => return,
    };
    if !text.is_empty() {
        emit(
            out,
            json!({ "type": "content_block_delta", "index": index, "delta": delta }),
        );
    }
}

/// Write the event `v`, named by its type.
pub(super) fn emit(out: &mut BytesMut, v: Value) {
    out.extend_from_slice(event(v["type"].as_str(), &v).as_bytes());
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
            Output::Signature {
                block: Some(block),
                data,
            } => self.signature(*block, data, out),
            Output::Signature { block: None, data } => self.withheld(data, out),
            Output::BlockFields { block, native } if *native.format == *ANTHROPIC => {
                self.fields.push((*block, native.fields.clone()));
            }
            Output::BlockNative { block, native } if *native.format == *ANTHROPIC => {
                self.begin(out);
                // A block this format keeps takes its index.
                if native.value["type"] == "content_block_start" {
                    self.next = self.next.max(block.saturating_add(1));
                }
                let mut raw = Value::clone(&native.value);
                raw["index"] = (*block).into();
                emit(out, raw);
            }
            Output::Native(NativeValue { format, value: raw }) if **format == *ANTHROPIC => {
                self.begin(out);
                match raw["type"].as_str() {
                    // Beside the terminal `message_delta`, which carries them.
                    Some("message_delta") => self.stop.add(raw),
                    _ => emit(out, Value::clone(raw)),
                }
            }
            // Another format's: only a client of that format could read it.
            Output::BlockFields { .. } | Output::BlockNative { .. } | Output::Native(_) => {}
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
                None => AnthropicMessages::error_body(f).to_string(),
            };
            out.extend_from_slice(event(Some("error"), &body).as_bytes());
            return Ok(());
        }
        self.finish(out)?;
        emit(out, json!({ "type": "message_stop" }));
        Ok(())
    }
}
