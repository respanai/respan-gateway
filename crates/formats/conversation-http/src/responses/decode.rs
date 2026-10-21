//! Responses, streamed or whole, as events: the decoder.

use super::{
    ANNOTATION, RESPONSE_FIELDS, RESPONSES, USAGE_FIELDS, extra_usage, response_fields, usage_of,
};
use crate::schema::{
    Totals, ended, error_event, native_fields, rest_of, says, says_error, unreadable_event,
};
use respan_gateway_conversation::{Block, Finish, Output, Start};
use respan_gateway_core::Fault;
use respan_gateway_http::bytes::Bytes;
use respan_gateway_sse::event_json;
use serde_json::{Map, Value, json};
use std::sync::Arc;

/// Decodes events and whole bodies of Responses into events, as the module
/// says.
#[derive(Debug)]
pub struct ResponsesDecoder {
    name: Arc<str>,
    totals: Totals,
    /// The usage fields with no unit that were last said, to say them once.
    usage_extra: Map<String, Value>,
    started: bool,
    /// The response's fields as last said.
    said: Map<String, Value>,
    done: bool,
    /// The number of the next block.
    blocks: u32,
    items: Vec<Item>,
    /// Whether the response called a function.
    called: bool,
    /// The fields said of each block's item so far.
    fields_said: Vec<(u32, Map<String, Value>)>,
}

/// What an item that is done closes.
enum Closing {
    /// The first part's block, and the blocks still open.
    Message(Option<u32>, Vec<u32>),
    /// The block, whether it is open, and the arguments it lacks.
    Call(u32, bool, Option<String>),
    /// The block, whether it is open, and the summary it lacks.
    Reasoning(u32, bool, Option<String>),
}

impl Default for ResponsesDecoder {
    fn default() -> Self {
        Self {
            name: Arc::from(RESPONSES),
            totals: Totals::default(),
            usage_extra: Map::new(),
            started: false,
            said: Map::new(),
            done: false,
            blocks: 0,
            items: Vec::new(),
            called: false,
            fields_said: Vec::new(),
        }
    }
}

/// An output item as the stream has it so far, by its `output_index`.
#[derive(Debug)]
struct Item {
    index: u64,
    what: What,
}

#[derive(Debug)]
enum What {
    /// A message: its added event waits for its first part, and its parts
    /// are blocks.
    Message {
        added: Option<Value>,
        parts: Vec<Part>,
    },
    Call {
        block: u32,
        args: bool,
        open: bool,
    },
    Reasoning {
        block: u32,
        text: bool,
        open: bool,
        /// The summary part last begun.
        summary: Option<u64>,
    },
    /// An item the kind has no form for: all its events are native.
    Opaque,
}

#[derive(Debug)]
struct Part {
    content_index: u64,
    block: u32,
    open: bool,
    text: bool,
    annotations: usize,
}

fn index_of(v: &Value) -> u64 {
    v["output_index"].as_u64().unwrap_or(0)
}

impl ResponsesDecoder {
    fn native(&self, v: Value, out: &mut Vec<Output>) {
        out.push(Output::native(self.name.clone(), v));
    }

    fn item(&mut self, index: u64) -> Option<&mut Item> {
        self.items.iter_mut().find(|i| i.index == index)
    }

    fn next_block(&mut self) -> u32 {
        self.blocks += 1;
        self.blocks - 1
    }

    /// The response's start, once.
    fn start(&mut self, r: &Value, out: &mut Vec<Output>) {
        if std::mem::replace(&mut self.started, true) {
            return;
        }
        let (id, model) = (r["id"].as_str(), r["model"].as_str());
        let mut start = Start::new(id.unwrap_or_default(), model.unwrap_or_default());
        self.said = response_fields(r);
        start.native = native_fields(RESPONSES, self.said.clone());
        out.push(Output::Start(start));
    }

    /// The fields of `item` beyond `taken`, as one native before its block,
    /// or after it if they changed: a done item says more than its added one.
    fn fields(&mut self, block: u32, item: &Value, taken: &[&str], out: &mut Vec<Output>) {
        // A status other than the two a stream passes through is said.
        let plain = matches!(
            item["status"].as_str(),
            Some("completed" | "in_progress") | None
        );
        let mut taken = taken.to_vec();
        if plain {
            taken.push("status");
        }
        let fields = rest_of(item, &taken);
        let said = self.fields_said.iter_mut().find(|(b, _)| *b == block);
        match said {
            Some((_, said)) if *said == fields => return,
            Some((_, said)) => *said = fields.clone(),
            None => self.fields_said.push((block, fields.clone())),
        }
        if !fields.is_empty() {
            out.push(Output::block_fields(RESPONSES, block, fields));
        }
    }

    /// The annotations of `part` past the first `seen`, as natives of `block`.
    fn annotations(&self, block: u32, part: &Value, seen: usize, out: &mut Vec<Output>) -> usize {
        let all = part["annotations"].as_array().into_iter().flatten();
        let mut count = seen;
        for annotation in all.skip(seen) {
            let v = json!({ "type": ANNOTATION, "block": block, "annotation": annotation });
            self.native(v, out);
            count += 1;
        }
        count
    }

    /// An item the kind has no form for, whole, as its done event.
    fn opaque_whole(&mut self, index: u64, item: &Value, out: &mut Vec<Output>) {
        self.items.push(Item {
            index,
            what: What::Opaque,
        });
        let v = json!({ "type": "response.output_item.done", "output_index": index, "item": item });
        self.native(v, out);
    }

    /// An item whole, as its events: a body's, or a stream's that skipped its
    /// parts.
    fn item_whole(&mut self, index: u64, item: &Value, out: &mut Vec<Output>) {
        match item["type"].as_str() {
            Some("message") => self.message_whole(index, item, out),
            Some("function_call") => match (item["call_id"].as_str(), item["name"].as_str()) {
                (Some(id), Some(name)) => {
                    let block = self.next_block();
                    let taken = ["type", "call_id", "name", "arguments"];
                    self.fields(block, item, &taken, out);
                    out.push(Output::block_start(block, call(id, name)));
                    if let Some(args) = item["arguments"].as_str().filter(|a| !a.is_empty()) {
                        out.push(Output::arguments(block, args));
                    }
                    out.push(Output::block_end(block));
                    self.called = true;
                    self.items.push(Item {
                        index,
                        what: What::Call {
                            block,
                            args: true,
                            open: false,
                        },
                    });
                }
                _ => self.opaque_whole(index, item, out),
            },
            Some("reasoning") if summary_texts(item).is_some() => {
                let block = self.next_block();
                self.fields(block, item, &["type", "summary"], out);
                out.push(Output::block_start(block, Block::Reasoning));
                let text = summary_texts(item).unwrap_or_default().join("\n\n");
                if !text.is_empty() {
                    out.push(Output::reasoning(block, text));
                }
                out.push(Output::block_end(block));
                self.items.push(Item {
                    index,
                    what: What::Reasoning {
                        block,
                        text: true,
                        open: false,
                        summary: None,
                    },
                });
            }
            _ => self.opaque_whole(index, item, out),
        }
    }

    fn message_whole(&mut self, index: u64, item: &Value, out: &mut Vec<Output>) {
        let parts = item["content"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        let output_text = |p: &Value| p["type"] == "output_text" && p["text"].is_string();
        if item["role"] != "assistant" || parts.is_empty() || !parts.iter().all(output_text) {
            return self.opaque_whole(index, item, out);
        }
        let mut kept = Vec::new();
        for (i, part) in parts.iter().enumerate() {
            let block = self.next_block();
            if i == 0 {
                self.fields(block, item, &["type", "role", "content"], out);
            }
            out.push(Output::block_start(block, Block::Text));
            if let Some(text) = part["text"].as_str().filter(|t| !t.is_empty()) {
                out.push(Output::text(block, text));
            }
            let annotations = self.annotations(block, part, 0, out);
            out.push(Output::block_end(block));
            kept.push(Part {
                content_index: u64::try_from(i).unwrap_or(u64::MAX),
                block,
                open: false,
                text: true,
                annotations,
            });
        }
        self.items.push(Item {
            index,
            what: What::Message {
                added: None,
                parts: kept,
            },
        });
    }

    /// Ends every block still open.
    fn close_all(&mut self, out: &mut Vec<Output>) {
        for item in &mut self.items {
            match &mut item.what {
                What::Message { parts, .. } => {
                    for part in parts.iter_mut().filter(|p| p.open) {
                        part.open = false;
                        out.push(Output::block_end(part.block));
                    }
                }
                What::Call { block, open, .. } | What::Reasoning { block, open, .. } => {
                    if std::mem::take(open) {
                        out.push(Output::block_end(*block));
                    }
                }
                What::Opaque => {}
            }
        }
    }

    /// The end of the response `r`: its usage, the fields it changed and
    /// why it stopped.
    fn finish(&mut self, r: &Value, out: &mut Vec<Output>) -> Result<(), Fault> {
        self.close_all(out);
        let usage = self.totals.increment(&usage_of(&r["usage"])?);
        if !usage.is_empty() {
            out.push(Output::Usage(usage));
        }
        let extra = extra_usage(&r["usage"]);
        if !extra.is_empty() && extra != self.usage_extra {
            let v = json!({ "type": USAGE_FIELDS, "fields": extra });
            self.native(v, out);
            self.usage_extra = extra;
        }
        let mut changed = Map::new();
        for (key, value) in response_fields(r) {
            if self.said.get(&key) != Some(&value) {
                self.said.insert(key.clone(), value.clone());
                changed.insert(key, value);
            }
        }
        if !changed.is_empty() {
            let v = json!({ "type": RESPONSE_FIELDS, "fields": changed });
            self.native(v, out);
        }
        let finish = match r["status"].as_str() {
            Some("completed") if self.called => Finish::ToolUse,
            Some("completed") => Finish::Stop,
            Some("incomplete") => match r["incomplete_details"]["reason"].as_str() {
                Some("max_output_tokens") => Finish::Length,
                Some("content_filter") => Finish::Filtered,
                Some(reason) => Finish::native(RESPONSES, reason),
                None => Finish::native(RESPONSES, "incomplete"),
            },
            Some(other) => {
                let message = format!("the response ended as '{other}', not completed");
                return Err(Fault::upstream(message));
            }
            None => return Err(Fault::upstream("the response has no status")),
        };
        out.push(Output::finished(finish));
        Ok(())
    }

    fn added(&mut self, v: &Value, out: &mut Vec<Output>) {
        let (index, item) = (index_of(v), &v["item"]);
        match item["type"].as_str() {
            Some("message") => self.items.push(Item {
                index,
                what: What::Message {
                    added: Some(item.clone()),
                    parts: Vec::new(),
                },
            }),
            Some("function_call") if item["call_id"].is_string() && item["name"].is_string() => {
                let block = self.next_block();
                let id = item["call_id"].as_str().unwrap_or_default();
                let name = item["name"].as_str().unwrap_or_default();
                let taken = ["type", "call_id", "name", "arguments"];
                self.fields(block, item, &taken, out);
                out.push(Output::block_start(block, call(id, name)));
                let args = item["arguments"].as_str().filter(|a| !a.is_empty());
                if let Some(args) = args {
                    out.push(Output::arguments(block, args));
                }
                self.called = true;
                self.items.push(Item {
                    index,
                    what: What::Call {
                        block,
                        args: args.is_some(),
                        open: true,
                    },
                });
            }
            Some("reasoning") => {
                let block = self.next_block();
                self.fields(block, item, &["type", "summary"], out);
                out.push(Output::block_start(block, Block::Reasoning));
                self.items.push(Item {
                    index,
                    what: What::Reasoning {
                        block,
                        text: false,
                        open: true,
                        summary: None,
                    },
                });
            }
            _ => {
                self.items.push(Item {
                    index,
                    what: What::Opaque,
                });
                self.native(v.clone(), out);
            }
        }
    }

    fn part_added(&mut self, v: &Value, out: &mut Vec<Output>) {
        let (index, content_index) = (index_of(v), v["content_index"].as_u64().unwrap_or(0));
        let part = &v["part"];
        if self.item(index).is_none() {
            self.items.push(Item {
                index,
                what: What::Message {
                    added: None,
                    parts: Vec::new(),
                },
            });
        }
        let text = part["type"] == "output_text";
        let block = self.blocks;
        let Some(Item {
            what: What::Message { added, parts },
            ..
        }) = self.item(index)
        else {
            return self.native(v.clone(), out);
        };
        if !text {
            // The item holds more than the kind has a form for.
            let pending = added.take();
            let blocks: Vec<_> = parts.iter().filter(|p| p.open).map(|p| p.block).collect();
            if let Some(item) = self.item(index) {
                item.what = What::Opaque;
            }
            blocks
                .into_iter()
                .for_each(|b| out.push(Output::block_end(b)));
            if let Some(added) = pending {
                self.native(added, out);
            }
            return self.native(v.clone(), out);
        }
        let first = added.take();
        parts.push(Part {
            content_index,
            block,
            open: true,
            text: false,
            annotations: 0,
        });
        self.blocks += 1;
        if let Some(item) = first {
            self.fields(block, &item, &["type", "role", "content"], out);
        }
        out.push(Output::block_start(block, Block::Text));
        if let Some(initial) = part["text"].as_str().filter(|t| !t.is_empty()) {
            out.push(Output::text(block, initial));
            if let Some(p) = self.part(index, content_index) {
                p.text = true;
            }
        }
        let seen = self.annotations(block, part, 0, out);
        if let Some(p) = self.part(index, content_index) {
            p.annotations = seen;
        }
    }

    fn part(&mut self, index: u64, content_index: u64) -> Option<&mut Part> {
        match &mut self.item(index)?.what {
            What::Message { parts, .. } => {
                parts.iter_mut().find(|p| p.content_index == content_index)
            }
            _ => None,
        }
    }

    fn part_done(&mut self, v: &Value, out: &mut Vec<Output>) {
        let (index, content_index) = (index_of(v), v["content_index"].as_u64().unwrap_or(0));
        let Some(part) = self.part(index, content_index) else {
            return self.native(v.clone(), out);
        };
        let (block, open, had_text, seen) = (part.block, part.open, part.text, part.annotations);
        if !open {
            return;
        }
        part.text = true;
        if let Some(text) = v["part"]["text"]
            .as_str()
            .filter(|t| !t.is_empty() && !had_text)
        {
            out.push(Output::text(block, text));
        }
        let seen = self.annotations(block, &v["part"], seen, out);
        if let Some(part) = self.part(index, content_index) {
            part.annotations = seen;
        }
    }

    /// The item is done: its last fields, the rest of what it says, and the
    /// end of its blocks.
    fn item_done(&mut self, v: &Value, out: &mut Vec<Output>) {
        let (index, item) = (index_of(v), &v["item"]);
        let closing = match self.item(index).map(|known| &mut known.what) {
            None => return self.item_whole(index, item, out),
            Some(What::Opaque) => return self.native(v.clone(), out),
            Some(What::Message { parts, .. }) => {
                let first = parts.first().map(|p| p.block);
                let open = parts.iter_mut().filter(|p| p.open);
                let open: Vec<u32> = open
                    .map(|p| {
                        p.open = false;
                        p.block
                    })
                    .collect();
                Closing::Message(first, open)
            }
            Some(What::Call { block, args, open }) => {
                let arguments = item["arguments"].as_str().filter(|a| !a.is_empty());
                let late = arguments.filter(|_| !*args);
                *args |= late.is_some();
                Closing::Call(*block, std::mem::take(open), late.map(str::to_owned))
            }
            Some(What::Reasoning {
                block, text, open, ..
            }) => {
                let texts = summary_texts(item).filter(|_| !*text);
                let late = texts
                    .map(|t| {
                        t.join(
                            "

",
                        )
                    })
                    .filter(|t| !t.is_empty());
                Closing::Reasoning(*block, std::mem::take(open), late)
            }
        };
        match closing {
            Closing::Message(first, open) => {
                if let Some(first) = first {
                    self.fields(first, item, &["type", "role", "content"], out);
                }
                open.iter().for_each(|b| out.push(Output::block_end(*b)));
                let content = item["content"].as_array();
                if first.is_none() && content.is_some_and(|c| !c.is_empty()) {
                    self.items.retain(|i| i.index != index);
                    self.message_whole(index, item, out);
                }
            }
            Closing::Call(block, open, late) => {
                self.fields(block, item, &["type", "call_id", "name", "arguments"], out);
                out.extend(late.map(|a| Output::arguments(block, a)));
                if open {
                    out.push(Output::block_end(block));
                }
            }
            Closing::Reasoning(block, open, late) => {
                self.fields(block, item, &["type", "summary"], out);
                out.extend(late.map(|t| Output::reasoning(block, t)));
                if open {
                    out.push(Output::block_end(block));
                }
            }
        }
    }

    /// The block a text event `v` names.
    fn text_block(&mut self, v: &Value) -> Option<u32> {
        let (index, content_index) = (index_of(v), v["content_index"].as_u64().unwrap_or(0));
        self.part(index, content_index)
            .filter(|p| p.open)
            .map(|p| p.block)
    }

    fn delta(&mut self, v: &Value, out: &mut Vec<Output>) {
        let (index, content_index) = (index_of(v), v["content_index"].as_u64().unwrap_or(0));
        // A message's text streamed without its part's added event, as some
        // vendors send it: its first delta opens the part.
        let message = matches!(
            self.item(index),
            Some(Item {
                what: What::Message { .. },
                ..
            })
        );
        if message && self.part(index, content_index).is_none() {
            let added = json!({
                "type": "response.content_part.added", "output_index": index,
                "content_index": content_index, "part": { "type": "output_text", "text": "" },
            });
            self.part_added(&added, out);
        }
        let text = v["delta"].as_str();
        match (self.text_block(v), text) {
            (Some(block), Some(text)) => {
                if !text.is_empty() {
                    out.push(Output::text(block, text));
                    if let Some(p) = self.part(index, content_index) {
                        p.text = true;
                    }
                }
            }
            _ => self.native(v.clone(), out),
        }
    }

    fn text_done(&mut self, v: &Value, out: &mut Vec<Output>) {
        let (index, content_index) = (index_of(v), v["content_index"].as_u64().unwrap_or(0));
        let Some(part) = self.part(index, content_index).filter(|p| p.open) else {
            return;
        };
        let block = part.block;
        let said = std::mem::replace(&mut part.text, true);
        if let Some(text) = v["text"].as_str().filter(|t| !t.is_empty() && !said) {
            out.push(Output::text(block, text));
        }
    }

    fn annotation(&mut self, v: &Value, out: &mut Vec<Output>) {
        let (index, content_index) = (index_of(v), v["content_index"].as_u64().unwrap_or(0));
        match self.part(index, content_index).filter(|p| p.open) {
            Some(part) => {
                part.annotations += 1;
                let block = part.block;
                let n =
                    json!({ "type": ANNOTATION, "block": block, "annotation": v["annotation"] });
                self.native(n, out);
            }
            None => self.native(v.clone(), out),
        }
    }

    fn call_block(&mut self, index: u64) -> Option<(u32, &mut bool)> {
        match &mut self.item(index)?.what {
            What::Call { block, args, .. } => Some((*block, args)),
            _ => None,
        }
    }

    fn arguments(&mut self, v: &Value, done: bool, out: &mut Vec<Output>) {
        let key = if done { "arguments" } else { "delta" };
        let Some((block, args)) = self.call_block(index_of(v)) else {
            return self.native(v.clone(), out);
        };
        let Some(text) = v[key].as_str() else {
            return;
        };
        // A done event repeats what the deltas said.
        let repeated = done && *args;
        if !text.is_empty() && !repeated {
            *args = true;
            out.push(Output::arguments(block, text));
        }
    }

    fn summary(&mut self, v: &Value, kind: &str, out: &mut Vec<Output>) {
        let Some(Item {
            what:
                What::Reasoning {
                    block,
                    text,
                    summary,
                    ..
                },
            ..
        }) = self.item(index_of(v))
        else {
            return self.native(v.clone(), out);
        };
        let block = *block;
        let part = v["summary_index"].as_u64().unwrap_or(0);
        match kind {
            "added" => {
                // Summary parts are joined as paragraphs.
                if *text && summary.is_some_and(|s| s != part) {
                    out.push(Output::reasoning(block, "\n\n"));
                }
                *summary = Some(part);
            }
            _ => {
                let said = v["delta"].as_str().filter(|d| !d.is_empty());
                if let Some(said) = said {
                    *text = true;
                    out.push(Output::reasoning(block, said));
                }
            }
        }
    }
}

fn call(id: &str, name: &str) -> Block {
    Block::ToolCall {
        id: id.into(),
        name: name.into(),
    }
}

/// The texts of a reasoning item's summary, if each part is a summary text.
fn summary_texts(item: &Value) -> Option<Vec<&str>> {
    let parts = item["summary"].as_array()?;
    fn text(p: &Value) -> Option<&str> {
        match p["type"].as_str()? {
            "summary_text" => p["text"].as_str(),
            _ => None,
        }
    }
    parts.iter().map(text).collect()
}

/// The fault of a failure whose provider error body is `error`: the body,
/// not the event it came in, goes to a client of this format.
fn failure(error: &Value) -> Fault {
    let data = format!("data: {error}\n\n");
    error_event(data.as_bytes(), error)
}

/// The error `e` of a response or error event, as a provider error body.
fn error_body(e: &Value) -> Value {
    let mut error = Map::new();
    for key in ["code", "message", "param"] {
        if says(&e[key]) {
            error.insert(key.into(), e[key].clone());
        }
    }
    json!({ "error": error })
}

impl ResponsesDecoder {
    /// The events of one stream event, `raw`.
    pub(crate) fn event(&mut self, raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
        if self.done {
            return Ok(());
        }
        let Some(v) = event_json(&raw) else {
            unreadable_event(&self.name, &raw, out);
            return Ok(());
        };
        if says_error(&v) {
            return Err(error_event(&raw, &v));
        }
        let kind = v["type"].as_str().unwrap_or_default();
        // An item the kind has no form for is native in all its events.
        let opaque = v.get("output_index").is_some()
            && !kind.ends_with("output_item.added")
            && matches!(
                self.item(index_of(&v)),
                Some(Item {
                    what: What::Opaque,
                    ..
                })
            );
        if opaque {
            self.native(v, out);
            return Ok(());
        }
        match kind {
            "error" => return Err(failure(&error_body(&v))),
            "response.failed" => {
                let r = &v["response"];
                let error = match says(&r["error"]) {
                    true => error_body(&r["error"]),
                    false => json!({ "error": { "message": "the response failed" } }),
                };
                return Err(failure(&error));
            }
            "response.created" | "response.in_progress" | "response.queued" => {
                self.start(&v["response"], out);
            }
            "response.completed" | "response.incomplete" => {
                self.start(&v["response"], out);
                self.finish(&v["response"], out)?;
                self.done = true;
            }
            "response.output_item.added" => self.added(&v, out),
            "response.content_part.added" => self.part_added(&v, out),
            "response.output_text.delta" => self.delta(&v, out),
            "response.output_text.done" => self.text_done(&v, out),
            "response.output_text.annotation.added" => self.annotation(&v, out),
            "response.content_part.done" => self.part_done(&v, out),
            "response.function_call_arguments.delta" => self.arguments(&v, false, out),
            "response.function_call_arguments.done" => self.arguments(&v, true, out),
            "response.reasoning_summary_part.added" => self.summary(&v, "added", out),
            "response.reasoning_summary_text.delta" => self.summary(&v, "delta", out),
            "response.reasoning_summary_text.done" | "response.reasoning_summary_part.done" => {}
            "response.output_item.done" => self.item_done(&v, out),
            _ => self.native(v, out),
        }
        Ok(())
    }

    /// The events of a stream that ended well.
    fn end_stream(&mut self, _out: &mut Vec<Output>) -> Result<(), Fault> {
        ended(self.done)
    }

    /// The events of one whole response body `v`.
    pub(crate) fn body(&mut self, v: &Value, out: &mut Vec<Output>) -> Result<(), Fault> {
        match v["status"].as_str() {
            Some("failed") => {
                let error = match says(&v["error"]) {
                    true => error_body(&v["error"]),
                    false => json!({ "error": { "message": "the response failed" } }),
                };
                return Err(failure(&error));
            }
            Some("in_progress" | "queued") => {
                return Err(Fault::upstream("the response is not complete"));
            }
            _ => {}
        }
        self.start(v, out);
        for (i, item) in v["output"].as_array().into_iter().flatten().enumerate() {
            self.item_whole(u64::try_from(i).unwrap_or(u64::MAX), item, out);
        }
        self.finish(v, out)?;
        self.done = true;
        Ok(())
    }
}

impl respan_gateway_http::Decoder<Output> for ResponsesDecoder {
    fn decode(&mut self, raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
        self.event(raw, out)
    }

    fn end(&mut self, end: Result<(), Fault>, out: &mut Vec<Output>) -> Result<(), Fault> {
        end?;
        self.end_stream(out)
    }
}
