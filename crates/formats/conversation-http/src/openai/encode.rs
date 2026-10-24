//! Events as OpenAI Chat, whole or streamed: the encoders.

use crate::schema::{
    Gate, NativeFieldsExt, Schema, check_usage, fits, merge_into, unix_now, unwritable_content,
};
use respan_gateway_conversation::{Block, Delta, Finish, Output, Start, Usage, block_fields};
use respan_gateway_core::{Fault, NativeValue};
use respan_gateway_http::Encoder;
use respan_gateway_http::bytes::{Bytes, BytesMut};
use respan_gateway_sse::event;
use serde_json::{Map, Value, json};

use super::{FINISH, Fields, OPENAI, OpenAiChat, USAGE, response_of, usage_body, usage_fields};

/// What a tool call's block fields say: the block, the entry's fields and
/// the function's.
pub(super) fn call_fields_of(raw: &Value) -> Option<(u32, Fields, Fields)> {
    let (block, fields) = block_fields(raw)?;
    let mut entry = fields.clone();
    let function = match entry.shift_remove("function") {
        Some(Value::Object(function)) => function,
        _ => Fields::new(),
    };
    Some((block, entry, function))
}

/// Put a tool call's block fields ([`call_fields_of`]) into the call `call`.
pub(super) fn merge_call(
    call: &mut Value,
    entry: &Map<String, Value>,
    function: &Map<String, Value>,
) {
    if let Some(call) = call.as_object_mut() {
        call.extend(entry.clone());
    }
    if let Some(f) = call["function"].as_object_mut() {
        f.extend(function.clone());
    }
}

/// The fields every chunk or body of a response has: id, `object`, `created`
/// (the response's, if it kept one), model and the fields of the start's own
/// `native` if it is in this format.
pub(super) fn envelope(start: Option<&Start>, object: &str, now: u64) -> Map<String, Value> {
    let (id, model) = start
        .map(|s| (s.id.as_str(), s.model.as_str()))
        .unwrap_or_default();
    let fields = start.filter(|s| s.native.format() == Some(OPENAI));
    let fields = fields.map(|s| s.native.fields());
    let created = fields.and_then(|f| f.get("created")).cloned();
    let mut v = Map::new();
    v.insert("id".into(), id.into());
    v.insert("object".into(), object.into());
    v.insert("created".into(), created.unwrap_or_else(|| now.into()));
    v.insert("model".into(), model.into());
    for (key, value) in fields.into_iter().flatten() {
        if key != "created" {
            v.insert(key.clone(), value.clone());
        }
    }
    v
}

/// A tool call as OpenAI writes it: in a chunk, numbered `index`, with its
/// id and name if it starts there; in a whole message, unnumbered.
pub(super) fn call_json(
    index: Option<u32>,
    id: Option<&str>,
    name: Option<&str>,
    arguments: &str,
) -> Value {
    let function = match name {
        Some(name) => json!({ "name": name, "arguments": arguments }),
        None => json!({ "arguments": arguments }),
    };
    let mut call = Map::new();
    if let Some(index) = index {
        call.insert("index".into(), index.into());
    }
    if let Some(id) = id {
        call.insert("id".into(), id.into());
        call.insert("type".into(), "function".into());
    }
    call.insert("function".into(), function);
    call.into()
}

/// `choice` with its body under `key`, as `delta` or `message`.
pub(super) fn rekey(choice: &Value, key: &str) -> Value {
    let mut choice = choice.clone();
    let other = if key == "delta" { "message" } else { "delta" };
    if let Some(fields) = choice.as_object_mut()
        && let Some(body) = fields.shift_remove(other)
    {
        fields.entry(key).or_insert(body);
    }
    choice
}

/// Put `more`, a choice of an [`Output::Native`], among a whole body's
/// `choices`: the first choice's fields join it, another is added. What
/// streamed in pieces is joined: strings are concatenated, and the arrays
/// of a message and of `logprobs` are appended.
pub(super) fn merge_choice(choices: &mut Vec<Value>, more: &Value) {
    let more = rekey(more, "message");
    let index = more["index"].as_u64().unwrap_or(0);
    let at = choices
        .iter()
        .position(|c| c["index"].as_u64() == Some(index));
    let (Some(at), Some(fields)) = (at, more.as_object()) else {
        return choices.push(more);
    };
    let choice = &mut choices[at];
    for (key, value) in fields {
        match (key.as_str(), value.as_object()) {
            ("index", _) => {}
            ("message", Some(body)) => {
                for (k, x) in body {
                    append_piece(&mut choice["message"][k], x);
                }
                let refused = choice["message"]["refusal"]
                    .as_str()
                    .is_some_and(|r| !r.is_empty());
                if refused && choice["message"]["content"] == "" {
                    choice["message"]["content"] = Value::Null;
                }
            }
            ("logprobs", Some(body)) => {
                for (k, x) in body {
                    append_piece(&mut choice["logprobs"][k], x);
                }
            }
            _ => choice[key] = value.clone(),
        }
    }
}

/// Join `piece` to what `into` has so far: a string continues, an array
/// grows, and anything else is replaced.
fn append_piece(into: &mut Value, piece: &Value) {
    match (into, piece) {
        (Value::String(so_far), Value::String(more)) => so_far.push_str(more),
        (Value::Array(so_far), Value::Array(more)) => so_far.extend(more.iter().cloned()),
        (into, piece) => *into = piece.clone(),
    }
}

/// Writes events as OpenAI chunks: the opening chunk, one per delta, and
/// at the end the finish, usage if the client asked for it, and `[DONE]`.
#[derive(Debug)]
pub(super) struct Chunks {
    /// Whether the client asked for the usage chunk; else it is hidden.
    show_usage: bool,
    start: Option<Start>,
    /// When the opening chunk was written.
    created: u64,
    began: bool,
    /// The blocks seen, by number.
    blocks: Vec<Slot>,
    /// How many tool calls have started: the next one's `index`.
    calls: u32,
    usage: Usage,
    /// The finish, written at the end, after the content released before it.
    finish: Option<Finish>,
    gate: Gate,
    /// The vendor fields of usage, written in the usage chunk.
    usage_extra: Map<String, Value>,
    /// The fields of tool calls that have not started, by block.
    call_fields: Vec<(u32, Fields, Fields)>,
    /// The response's fields a later chunk changed, for the next chunk.
    response: Fields,
}

#[derive(Debug)]
struct Slot {
    block: u32,
    kind: Block,
    /// A tool call's `index`.
    call: Option<u32>,
    /// Whether it had arguments: else it ends with `{}`.
    arguments: bool,
    ended: bool,
}

impl Chunks {
    pub(super) fn new(show_usage: bool) -> Self {
        Self {
            show_usage,
            start: None,
            created: 0,
            began: false,
            blocks: Vec::new(),
            calls: 0,
            usage: Usage::default(),
            finish: None,
            gate: Gate::default(),
            usage_extra: Map::new(),
            call_fields: Vec::new(),
            response: Map::new(),
        }
    }

    fn begin(&mut self, out: &mut BytesMut) {
        if std::mem::replace(&mut self.began, true) {
            return;
        }
        self.created = unix_now();
        let delta = json!({ "role": "assistant", "content": "" });
        self.chunk(
            out,
            json!([{ "index": 0, "delta": delta, "finish_reason": null }]),
            None,
        );
    }

    fn delta(&mut self, out: &mut BytesMut, delta: Value) {
        self.chunk(
            out,
            json!([{ "index": 0, "delta": delta, "finish_reason": null }]),
            None,
        );
    }

    fn chunk(&mut self, out: &mut BytesMut, choices: Value, usage: Option<Value>) {
        let mut chunk = envelope(self.start.as_ref(), "chat.completion.chunk", self.created);
        merge_into(&mut chunk, &std::mem::take(&mut self.response));
        chunk.insert("choices".into(), choices);
        if let Some(usage) = usage {
            chunk.insert("usage".into(), usage);
        }
        out.extend_from_slice(event(None, Value::Object(chunk)).as_bytes());
    }

    fn slot(&mut self, block: u32) -> Result<&mut Slot, Fault> {
        let slot = self.blocks.iter_mut().find(|s| s.block == block);
        slot.filter(|s| !s.ended).ok_or_else(unwritable_content)
    }

    fn open(&mut self, block: u32, kind: &Block, out: &mut BytesMut) -> Result<(), Fault> {
        if let Some(had) = self.blocks.iter().find(|s| s.block == block) {
            // A start repeated for an open block opens nothing more.
            return match had.kind == *kind && !had.ended {
                true => Ok(()),
                false => Err(unwritable_content()),
            };
        }
        self.begin(out);
        let call = match kind {
            Block::ToolCall { id, name } => {
                let index = self.calls;
                self.calls += 1;
                let mut call = call_json(Some(index), Some(id), Some(name), "");
                if let Some(at) = self.call_fields.iter().position(|(b, ..)| *b == block) {
                    let (_, entry, function) = self.call_fields.remove(at);
                    merge_call(&mut call, &entry, &function);
                }
                self.delta(out, json!({ "tool_calls": [call] }));
                Some(index)
            }
            _ => None,
        };
        self.blocks.push(Slot {
            block,
            kind: kind.clone(),
            call,
            arguments: false,
            ended: false,
        });
        Ok(())
    }

    fn say(&mut self, block: u32, part: &Delta, out: &mut BytesMut) -> Result<(), Fault> {
        let slot = self.slot(block)?;
        if !fits(part, &slot.kind) {
            return Err(unwritable_content());
        }
        let text = part.as_str();
        if text.is_empty() {
            return Ok(());
        }
        let delta = match (part, slot.call) {
            (Delta::Reasoning(_), _) => json!({ "reasoning_content": text }),
            (Delta::Text(_), _) => json!({ "content": text }),
            (Delta::Arguments(_), Some(index)) => {
                slot.arguments = true;
                json!({ "tool_calls": [call_json(Some(index), None, None, text)] })
            }
            _ => return Err(unwritable_content()),
        };
        self.delta(out, delta);
        Ok(())
    }

    /// End a block: a call that had no arguments has none, as JSON.
    fn close(&mut self, block: u32, out: &mut BytesMut) -> Result<(), Fault> {
        let slot = self.blocks.iter_mut().find(|s| s.block == block);
        let slot = slot.ok_or_else(unwritable_content)?;
        let bare = slot.call.filter(|_| !slot.arguments && !slot.ended);
        (slot.arguments, slot.ended) = (true, true);
        if let Some(index) = bare {
            let call = call_json(Some(index), None, None, "{}");
            self.delta(out, json!({ "tool_calls": [call] }));
        }
        Ok(())
    }
}

impl Chunks {
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

impl Encoder<Output> for Chunks {
    fn encode(&mut self, reply: &Output, frames: &mut Vec<Bytes>) -> Result<(), Fault> {
        crate::schema::frame(frames, |out| self.encode_into(reply, out))
    }

    fn end(&mut self, end: Result<(), Fault>, frames: &mut Vec<Bytes>) -> Result<(), Fault> {
        crate::schema::frame(frames, |out| self.end_into(&end, out))
    }
}

impl Chunks {
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
            Output::Native(NativeValue { format, value: raw }) if **format == *OPENAI => {
                self.write_native(raw, out);
            }
            // Another format's: only a client of that format could read it.
            Output::Native(_) => {}
            _ => return Err(unwritable_content()),
        }
        Ok(())
    }

    /// An event of this format's own: usage, call and response fields are
    /// kept for their place, a chunk is written as it came.
    fn write_native(&mut self, raw: &Value, out: &mut BytesMut) {
        if let Some(fields) = usage_fields(raw) {
            self.usage_extra.extend(fields.clone());
        } else if let Some(fields) = response_of(raw) {
            merge_into(&mut self.response, fields);
        } else if let Some((block, entry, function)) = call_fields_of(raw) {
            self.call_fields
                .push((block, entry.clone(), function.clone()));
        } else {
            match raw {
                // An event that could not be read, as it came.
                Value::String(data) => out.extend_from_slice(event(None, data).as_bytes()),
                v if v["choices"].is_array() => {
                    self.begin(out);
                    let choices = v["choices"].as_array().into_iter().flatten();
                    let choices: Vec<Value> = choices.map(|c| rekey(c, "delta")).collect();
                    self.chunk(out, choices.into(), None);
                }
                other => out.extend_from_slice(event(None, other).as_bytes()),
            }
        }
    }

    fn write_end(&mut self, end: &Result<(), Fault>, out: &mut BytesMut) -> Result<(), Fault> {
        if let Err(f) = end {
            // What the provider finished before it broke off is kept.
            if let Some(finish) = self.finish.take() {
                self.begin(out);
                let reason = FINISH.reason(&finish)?;
                self.chunk(
                    out,
                    json!([{ "index": 0, "delta": {}, "finish_reason": reason }]),
                    None,
                );
            }
            let body = match f
                .get::<respan_gateway_http::raw::RawReply>()
                .map(|r| r.0.body())
            {
                Some(raw) => String::from_utf8_lossy(raw).into_owned(),
                None => OpenAiChat::error_body(f).to_string(),
            };
            out.extend_from_slice(event(None, body).as_bytes());
            return Ok(());
        }
        self.begin(out);
        let open: Vec<u32> = self
            .blocks
            .iter()
            .filter(|s| !s.ended)
            .map(|s| s.block)
            .collect();
        for block in open {
            self.close(block, out)?;
        }
        let finish = self.finish.take().unwrap_or(Finish::Stop);
        let reason = FINISH.reason(&finish)?;
        self.chunk(
            out,
            json!([{ "index": 0, "delta": {}, "finish_reason": reason }]),
            None,
        );
        if self.show_usage && !self.usage.is_empty() {
            self.chunk(
                out,
                json!([]),
                Some(usage_body(&self.usage, &self.usage_extra)),
            );
        }
        out.extend_from_slice(b"data: [DONE]\n\n");
        Ok(())
    }
}
