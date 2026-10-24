//! OpenAI Chat, streamed or whole, as events: the decoder.

use crate::schema::{
    Totals, ended, error_event, native_fields, remainder, says, says_error, unreadable_event,
    unsupported_fields,
};
use respan_gateway_conversation::{Block, Delta, Finish, Output, Start};
use respan_gateway_core::Fault;
use respan_gateway_http::bytes::Bytes;
use respan_gateway_sse::event_data;
use serde_json::{Map, Value, json};
use std::sync::Arc;

use super::encode::rekey;
use super::{FINISH, Fields, OPENAI, extra_usage, usage_of};

/// Decodes chunks and whole bodies of OpenAI Chat into events, as the
/// module says.
#[derive(Debug)]
pub struct OpenAiDecoder {
    /// The format of what the kind has no form for: [`OPENAI`].
    name: Arc<str>,
    totals: Totals,
    started: bool,
    /// The vendor fields of the usage last said, to say them once.
    usage_extra: Map<String, Value>,
    /// The response's fields that came before its start, as Azure's first
    /// chunk, which names no `id`, has.
    early: Map<String, Value>,
    /// The response's fields as last said, from its start on.
    said: Map<String, Value>,
    done: bool,
    /// Whether the response said its finish and its usage, which the call
    /// always asks for: a stream that ends well after both is whole,
    /// `[DONE]` or not, as some OpenAI-compatible servers leave it out.
    finished: bool,
    counted: bool,
    /// Every block opened so far; a block's number is its place.
    blocks: Vec<Opened>,
    /// Whether the response made a tool call.
    called: bool,
}

#[derive(Debug)]
struct Opened {
    key: Key,
    open: bool,
    /// The id of the tool call this block is.
    call_id: Option<String>,
}

/// What a block of a response is made of, in OpenAI's terms.
#[derive(Debug, PartialEq)]
enum Key {
    /// `reasoning_content`.
    Reasoning,
    Text,
    /// The tool call numbered so by `index`.
    Call(u64),
}

impl OpenAiDecoder {
    pub(super) fn new() -> Self {
        Self {
            name: Arc::from(OPENAI),
            totals: Totals::default(),
            started: false,
            usage_extra: Map::new(),
            early: Map::new(),
            said: Map::new(),
            done: false,
            finished: false,
            counted: false,
            blocks: Vec::new(),
            called: false,
        }
    }
}

/// A tool call's part of a chunk or body.
struct Call {
    /// The entry's `index`, if it names one.
    index: Option<u64>,
    start: Option<(String, String)>,
    arguments: String,
    /// The entry's fields, and its function's, that the kind has no place
    /// for, even null or empty ones.
    entry: Map<String, Value>,
    function: Map<String, Value>,
}

/// The fields of tool call `block` the kind has no place for, the entry's,
/// and its function's under `function`, as its [`Output::block_fields`]:
/// before the call's block opens.
fn call_fields(block: u32, entry: &Fields, function: &Fields) -> Option<Output> {
    if entry.is_empty() && function.is_empty() {
        return None;
    }
    let mut fields = entry.clone();
    if !function.is_empty() {
        fields.insert("function".into(), Value::Object(function.clone()));
    }
    Some(Output::block_fields(OPENAI, block, fields))
}

impl OpenAiDecoder {
    /// The block of `key`, if it is open.
    fn open_block(&self, key: &Key) -> Option<u32> {
        let at = self.blocks.iter().position(|b| b.open && b.key == *key);
        at.and_then(|at| u32::try_from(at).ok())
    }

    /// The number of the tool call `call` is of: its `index`, else, for a
    /// vendor that sends none, the open call it continues (it names no id,
    /// or the open call's own) or a new one.
    fn call_index(&self, call: &Call) -> u64 {
        if let Some(index) = call.index {
            return index;
        }
        let calls = self.blocks.iter().filter(|b| matches!(b.key, Key::Call(_)));
        let named = call.start.as_ref().map(|(id, _)| id);
        match calls.clone().rfind(|b| b.open) {
            Some(Opened {
                key: Key::Call(index),
                call_id,
                ..
            }) if named.is_none() || named == call_id.as_ref() => *index,
            _ => u64::try_from(calls.count()).unwrap_or(u64::MAX),
        }
    }

    /// Open a block of `key` as `kind`, ending the open reasoning or text.
    fn open(&mut self, key: Key, kind: Block, out: &mut Vec<Output>) -> u32 {
        for (n, block) in self.blocks.iter_mut().enumerate() {
            if block.open && matches!(block.key, Key::Reasoning | Key::Text) {
                block.open = false;
                out.push(Output::block_end(u32::try_from(n).unwrap_or(u32::MAX)));
            }
        }
        let block = u32::try_from(self.blocks.len()).unwrap_or(u32::MAX);
        self.called |= matches!(kind, Block::ToolCall { .. });
        let call_id = match &kind {
            Block::ToolCall { id, .. } => Some(id.clone()),
            _ => None,
        };
        self.blocks.push(Opened {
            key,
            open: true,
            call_id,
        });
        out.push(Output::block_start(block, kind));
        block
    }

    /// Delta `delta` of the block of `key`, opened as `kind` if it is not.
    fn say(&mut self, key: Key, kind: Block, delta: Delta, out: &mut Vec<Output>) {
        let block = match self.open_block(&key) {
            Some(block) => block,
            None => self.open(key, kind, out),
        };
        out.push(Output::Delta { block, delta });
    }

    /// End every open block, in the order they opened.
    fn close(&mut self, out: &mut Vec<Output>) {
        for (n, block) in self.blocks.iter_mut().enumerate() {
            if std::mem::take(&mut block.open) {
                out.push(Output::block_end(u32::try_from(n).unwrap_or(u32::MAX)));
            }
        }
    }

    /// The events of one chunk, whose first choice's body is under `key`
    /// (`delta`, or `message` in a whole body).
    fn chunk(&mut self, v: &Value, key: &str, out: &mut Vec<Output>) -> Result<(), Fault> {
        let id = v["id"].as_str().filter(|id| !id.is_empty());
        let fields = response_fields(v);
        let mut changed = Map::new();
        match self.started {
            false => self.early.extend(fields),
            true => {
                for (key, value) in fields {
                    if self.said.get(&key) != Some(&value) {
                        self.said.insert(key.clone(), value.clone());
                        changed.insert(key, value);
                    }
                }
            }
        }
        if let (Some(id), false) = (id, self.started) {
            self.started = true;
            let model = v["model"].as_str().unwrap_or_default();
            let mut start = Start::new(id, model);
            self.said = std::mem::take(&mut self.early);
            start.native = native_fields(OPENAI, self.said.clone());
            out.push(Output::Start(start));
        }
        let u = &v["usage"];
        self.counted |= u.is_object();
        let usage = self.totals.increment(&usage_of(u)?);
        if !usage.is_empty() {
            out.push(Output::Usage(usage));
        }
        let native = extra_usage(u);
        if !native.is_empty() && native != self.usage_extra {
            out.push(Output::native(
                self.name.clone(),
                json!({ "usage": native }),
            ));
            self.usage_extra = native;
        }
        if !changed.is_empty() {
            let response = json!({ "response": changed });
            out.push(Output::native(self.name.clone(), response));
        }
        // What the kind has no form for goes first, so a chunk that cannot be
        // translated fails before any of it is written.
        for choice in v["choices"].as_array().into_iter().flatten() {
            if choice_index(choice) != 0 {
                out.push(Output::native(
                    self.name.clone(),
                    json!({ "choices": [rekey(choice, "delta")] }),
                ));
            }
        }
        let Some(first) = first_choice(v) else {
            return Ok(());
        };
        let body = &first[key];
        let calls = tool_calls(body);
        if let Some(rest) = choice_leftovers(first, key, calls.is_some()) {
            out.push(Output::native(
                self.name.clone(),
                json!({ "choices": [rest] }),
            ));
        }
        let reasoning = body["reasoning_content"].as_str();
        if let Some(text) = reasoning.filter(|t| !t.is_empty()) {
            let delta = Delta::Reasoning(text.into());
            self.say(Key::Reasoning, Block::Reasoning, delta, out);
        }
        if let Some(text) = body["content"].as_str().filter(|t| !t.is_empty()) {
            self.say(Key::Text, Block::Text, Delta::Text(text.into()), out);
        }
        for call in calls.into_iter().flatten() {
            let key = Key::Call(self.call_index(&call));
            let block = match (call.start, self.open_block(&key)) {
                (Some((id, name)), None) => {
                    let next = u32::try_from(self.blocks.len()).unwrap_or(u32::MAX);
                    out.extend(call_fields(next, &call.entry, &call.function));
                    self.open(key, Block::ToolCall { id, name }, out)
                }
                (_, Some(block)) => block,
                (None, None) => {
                    return Err(Fault::upstream(
                        "a tool call's arguments come before the call starts",
                    ));
                }
            };
            if !call.arguments.is_empty() {
                out.push(Output::arguments(block, call.arguments));
            }
        }
        if let Some(reason) = first["finish_reason"].as_str() {
            self.close(out);
            // A call the request forced finishes as "stop" at OpenAI; a
            // response that made calls ends in tool use all the same.
            let finish = match (FINISH.finish(reason), self.called) {
                (Finish::Stop, true) => Finish::ToolUse,
                (finish, _) => finish,
            };
            out.push(Output::finished(finish));
            self.finished = true;
        }
        Ok(())
    }
}

impl OpenAiDecoder {
    /// The events of one stream event, `raw`.
    pub(crate) fn event(&mut self, raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
        if self.done {
            return Ok(());
        }
        let data = event_data(&raw);
        if data.as_deref().map(str::trim) == Some("[DONE]") {
            self.close(out);
            self.done = true;
            return Ok(());
        }
        let v = data
            .as_deref()
            .and_then(|d| serde_json::from_str::<Value>(d).ok());
        let chunk = |v: &Value| v["choices"].is_array() || !v["usage"].is_null() || says_error(v);
        let Some(v) = v.filter(chunk) else {
            unreadable_event(&self.name, &raw, out);
            return Ok(());
        };
        if says_error(&v) {
            return Err(error_event(&raw, &v));
        }
        self.chunk(&v, "delta", out)
    }

    /// The events of a stream that ended well: whole once it said its
    /// finish and usage, else cut short.
    fn end_stream(&mut self, _out: &mut Vec<Output>) -> Result<(), Fault> {
        ended(self.done || self.finished && self.counted)
    }

    /// The events of one whole response body `v`.
    pub(crate) fn body(&mut self, v: &Value, out: &mut Vec<Output>) -> Result<(), Fault> {
        self.chunk(v, "message", out)?;
        self.close(out);
        self.done = true;
        Ok(())
    }
}

impl respan_gateway_http::Decoder<Output> for OpenAiDecoder {
    fn decode(&mut self, raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
        self.event(raw, out)
    }

    fn end(&mut self, end: Result<(), Fault>, out: &mut Vec<Output>) -> Result<(), Fault> {
        end?;
        self.end_stream(out)
    }
}

/// The fields of a chunk or body that are the response's, not an event's:
/// what is neither its id, model, choices nor usage.
fn response_fields(v: &Value) -> Map<String, Value> {
    let known = ["id", "model", "object", "choices", "usage", "obfuscation"];
    unsupported_fields(v, &known, &[])
}

/// A choice's `index`; a choice with none is the first.
fn choice_index(choice: &Value) -> u64 {
    choice["index"].as_u64().unwrap_or(0)
}

/// The first choice, if it is index 0: the conversation's.
fn first_choice(v: &Value) -> Option<&Value> {
    let choice = v["choices"].get(0)?;
    (choice_index(choice) == 0).then_some(choice)
}

/// The first choice without what the kind has: its text, tool calls
/// (`typed`, if the kind has all of them) and finish; its body is under
/// `delta`. `None` if nothing is left.
fn choice_leftovers(choice: &Value, key: &str, typed: bool) -> Option<Value> {
    let mut rest = Map::new();
    rest.insert("index".into(), 0.into());
    let fields = choice.as_object().into_iter().flatten();
    for (k, x) in fields {
        if k == "index" || k == "finish_reason" {
            continue;
        }
        if k != key {
            if says(x) {
                rest.insert(k.clone(), x.clone());
            }
            continue;
        }
        let mut body = Map::new();
        for (field, y) in x.as_object().into_iter().flatten() {
            let text = y.is_string() || y.is_null();
            let ours = match field.as_str() {
                "role" => true,
                "content" | "reasoning_content" => text,
                "tool_calls" => typed,
                _ => false,
            };
            if !ours && says(y) {
                body.insert(field.clone(), y.clone());
            }
        }
        if !body.is_empty() {
            rest.insert("delta".into(), body.into());
        }
    }
    (rest.len() > 1).then(|| rest.into())
}

/// The tool calls of a choice's `body`, if each is a function call the
/// kind can say, with the `index` each names. `None` leaves
/// them other content.
fn tool_calls(body: &Value) -> Option<Vec<Call>> {
    let calls = body.get("tool_calls")?.as_array()?;
    if calls.is_empty() {
        return None;
    }
    let call = |c: &Value| {
        let function = &c["function"];
        let function_call = c["type"].is_null() || c["type"] == "function";
        if !function_call {
            return None;
        }
        let arguments = match &function["arguments"] {
            Value::Null => "",
            arguments => arguments.as_str()?,
        };
        let index = match c.get("index") {
            Some(index) => Some(index.as_u64()?),
            None => None,
        };
        let start = c["id"].as_str().zip(function["name"].as_str());
        Some(Call {
            index,
            start: start.map(|(id, name)| (id.to_owned(), name.to_owned())),
            arguments: arguments.to_owned(),
            entry: remainder(c, &["index", "id", "type", "function"]),
            function: remainder(function, &["name", "arguments"]),
        })
    };
    calls.iter().map(call).collect()
}
