#![warn(missing_docs)]
//! The conversation kind: chats with a language model. Formats translate
//! through these types, and plugins read any conversation through them:
//! `cx.filter(|c: &mut Input| ..)` for the input and
//! `cx.filter(|r: &mut Output| ..)` for the output, one [`Output`] per event of
//! the reply: an `Output` is one event, not a whole reply.
//!
//! A reply is a sequence of events that follows the lifecycle of a
//! stream: [`Output::Start`], then for each content block a
//! [`Output::BlockStart`], its [`Output::Delta`]s (each a [`Delta`] of its
//! block's own kind) and an [`Output::BlockEnd`],
//! with [`Output::Usage`], [`Output::Finish`] and [`Output::Native`] among them.
//! A response that was not streamed decodes into the same sequence, and an
//! encoder collects the events into one body or writes them as a stream, so a
//! plugin handles one shape. [`Assembled`] folds the events into the whole
//! reply for plugins that need it.
//!
//! Tools are the kind's too: the tools a request offers ([`Tool`],
//! [`ToolChoice`]), the calls an assistant made and their results
//! ([`Content::ToolCall`], [`Content::ToolResult`]), and the calls a
//! response makes ([`Block::ToolCall`]), whose arguments stream as JSON text.
//!
//! What the kind has no form for stays as the format that decoded it has it
//! ([`NativeFields`], [`Content::Native`], [`Output::Native`]); it is sent only in that
//! format, and translating it is refused if it can change the answer.
//!
//! Values are plain data: they say what they hold and compare by it. They
//! keep nothing of the bytes they were decoded from.
//!
//! Direction is gateway-wide: [`Input`] travels from client to provider;
//! [`Output`] travels from provider to client, on either transport edge.
//!
//! The crate is the kind's types alone, with no I/O; its formats over HTTP
//! are `respan-gateway-conversation-http`'s.

use respan_gateway_core::Fault;

use serde_json::{Map, Value, json};
use std::collections::HashMap;

use respan_gateway_core::{FormatId, NativeFields, NativeValue};

mod media;
mod usage;
pub use media::{Media, MediaSource};
pub use usage::Usage;

/// Provider-neutral generation settings, independent of response framing.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct GenerationOptions {
    /// Maximum generated tokens.
    pub max_output_tokens: Option<u64>,
    /// Sampling temperature.
    pub temperature: Option<f64>,
    /// Nucleus sampling cutoff.
    pub top_p: Option<f64>,
    /// Sampling from the `top_k` likeliest tokens only.
    pub top_k: Option<u64>,
    /// How hard the model thinks before it answers.
    pub reasoning: Option<Reasoning>,
    /// Stop sequences.
    pub stop: Vec<String>,
    /// Tool selection policy.
    pub tool_choice: Option<ToolChoice>,
    /// Whether parallel tool calls are allowed.
    pub parallel_tool_calls: Option<bool>,
    /// Required shape of the generated answer.
    pub response_format: Option<ResponseFormat>,
}

/// How much the model thinks before it answers, and whether it says so.
///
/// A vendor states this as a named effort or as a token budget; the two do
/// not convert (a budget does not pick an effort, and the same effort buys
/// different thinking from different models), so a format that has only
/// one refuses the other. Only [`Effort::Off`] is the same in both. A plugin
/// that wants an effort as a budget decides it by its own policy, in a
/// filter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Reasoning {
    /// A named effort.
    pub effort: Option<Effort>,
    /// A limit on the thinking, in tokens.
    pub budget_tokens: Option<u64>,
    /// Whether the thinking is returned, summarized where the provider only
    /// summarizes it. `Some(false)` asks that it is withheld.
    pub summary: Option<bool>,
}

impl Reasoning {
    /// Thinking of this named `effort`.
    pub fn effort(effort: Effort) -> Self {
        Self {
            effort: Some(effort),
            ..Self::default()
        }
    }

    /// Thinking of at most `tokens`.
    pub fn budget(tokens: u64) -> Self {
        Self {
            budget_tokens: Some(tokens),
            ..Self::default()
        }
    }
}

/// A named amount of thinking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Effort {
    /// No thinking.
    Off,
    /// The least thinking.
    Minimal,
    /// Little thinking.
    Low,
    /// Moderate thinking.
    Medium,
    /// Much thinking.
    High,
    /// More thinking than [`Effort::High`].
    XHigh,
    /// The most thinking.
    Max,
}

/// A common structured-output requirement.
#[derive(Debug, Clone, PartialEq)]
pub enum ResponseFormat {
    /// Ordinary text.
    Text,
    /// A JSON object, without a supplied schema.
    JsonObject,
    /// JSON matching a named schema.
    JsonSchema {
        /// Schema name.
        name: String,
        /// Optional schema description.
        description: Option<String>,
        /// JSON Schema definition.
        schema: Value,
        /// Whether the provider must enforce the schema strictly.
        strict: Option<bool>,
    },
}

/// The kind's input: a conversation request.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct Input {
    /// The model asked for, if any.
    pub model: Option<String>,
    /// The messages, in order.
    pub messages: Vec<Message>,
    /// Generation behavior, independent of transport framing.
    pub options: GenerationOptions,
    /// The end user's id, as the client names them to the provider.
    pub user: Option<String>,
    /// The tools the model may call.
    pub tools: Vec<Tool>,
    /// The tools the provider runs itself while it answers.
    pub provider_tools: Vec<ProviderTool>,
    /// The request's fields the kind has no place for, such as tools a
    /// provider runs itself.
    pub native: Option<NativeFields>,
}

impl Input {
    /// A request of these `messages` alone, such as a judge's own prompt.
    pub fn new(messages: impl IntoIterator<Item = Message>) -> Self {
        Self {
            messages: messages.into_iter().collect(),
            ..Self::default()
        }
    }

    /// The model asked for, if any.
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Asks for `model`.
    pub fn set_model(&mut self, model: String) {
        self.model = Some(model);
    }
}

/// A filter that asks for `model`, such as `cx.filter(set_model(m))`.
pub fn set_model(
    model: impl Into<String>,
) -> impl FnMut(&mut Input) -> Result<(), Fault> + Clone + Send + 'static {
    let model = model.into();
    move |chat: &mut Input| {
        chat.model = Some(model.clone());
        Ok(())
    }
}

/// One message of a conversation: its author and what it says.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Message {
    /// Its author.
    pub role: Role,
    /// What it says, part by part.
    pub content: Vec<Content>,
    /// The message's fields the kind has no place for, such as a name. A
    /// [`Role::Native`] is in the format of `native`.
    pub native: Option<NativeFields>,
}

impl Message {
    /// A message of one part.
    pub fn new(role: Role, content: impl Into<Content>) -> Self {
        Self {
            role,
            content: vec![content.into()],
            native: None,
        }
    }

    /// The message's text parts, joined; tool calls, results and other
    /// content are skipped.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .map(|c| match c {
                Content::Text { text, .. } => text.as_str(),
                _ => "",
            })
            .collect()
    }

    /// Whether the kind has a place for all of it: a known role, no native
    /// fields on the message, and text parts (whose own native fields, such as
    /// a cache marker, do not change what they say).
    pub fn is_plain(&self) -> bool {
        let text = |c: &Content| matches!(c, Content::Text { .. });
        !matches!(self.role, Role::Native { .. })
            && self.native.as_ref().is_none_or(NativeFields::is_empty)
            && self.content.iter().all(text)
    }
}

/// Who wrote a message.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Role {
    /// Instructions for the model.
    System,
    /// Instructions for the model with a developer's authority; a format that has no
    /// such role writes them as system.
    Developer,
    /// The person or program using the model.
    User,
    /// The model.
    Assistant,
    /// A role the kind has no form for, as `format` names it; only a call
    /// in that format can send it.
    #[non_exhaustive]
    Native {
        /// The format that names it.
        format: FormatId,
        /// The role, as that format names it.
        name: String,
    },
}

impl Role {
    /// The role `name` of `format`, which the kind has no form for.
    pub fn native(format: impl Into<FormatId>, name: impl Into<String>) -> Self {
        Role::Native {
            format: format.into(),
            name: name.into(),
        }
    }
}

/// A part of a message; a plugin that must see all of it refuses variants
/// it does not know.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Content {
    /// Text.
    #[non_exhaustive]
    Text {
        /// The text.
        text: String,
        /// The part's fields the kind has no place for, such as a cache
        /// marker.
        native: Option<NativeFields>,
    },
    /// Reasoning retained in conversation history.
    Reasoning {
        /// Reasoning text.
        text: String,
        /// Native fields such as a provider signature.
        native: Option<NativeFields>,
    },
    /// An image with a typed source and MIME type.
    Image(Media),
    /// Audio with a typed source and MIME type.
    Audio(Media),
    /// A document with a typed source and MIME type.
    Document(Media),
    /// A tool call an assistant made.
    #[non_exhaustive]
    ToolCall {
        /// The call's id, which its result names.
        id: String,
        /// The tool's name.
        name: String,
        /// Its arguments, as JSON text.
        arguments: String,
        /// The part's fields the kind has no place for.
        native: Option<NativeFields>,
    },
    /// What a tool answered a call.
    #[non_exhaustive]
    ToolResult {
        /// The id of the call it answers.
        id: String,
        /// What the tool said.
        content: Vec<Content>,
        /// Whether the call failed.
        is_error: bool,
        /// The part's fields the kind has no place for.
        native: Option<NativeFields>,
    },
    /// A part the kind has no form for, such as a provider-run tool's
    /// block, as its format has it; only a call in that format can send it.
    Native(NativeValue),
}

impl Content {
    /// A call of tool `name`, with its `id` and its `arguments` as JSON text.
    pub fn tool_call(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        Content::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
            native: None,
        }
    }

    /// The answer to call `id`: what the tool said, and whether it failed.
    pub fn tool_result(id: impl Into<String>, content: Vec<Content>, is_error: bool) -> Self {
        Content::ToolResult {
            id: id.into(),
            content,
            is_error,
            native: None,
        }
    }

    /// A part of `format` the kind has no form for.
    pub fn native(format: impl Into<FormatId>, raw: impl Into<Value>) -> Self {
        Content::Native(NativeValue::new(format, raw))
    }
}

/// A tool a request offers the model.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Tool {
    /// Its name, which a call names.
    pub name: String,
    /// What it does, for the model.
    pub description: Option<String>,
    /// The JSON schema of its arguments.
    pub parameters: Value,
    /// Its fields the kind has no place for, such as a cache marker.
    pub native: Option<NativeFields>,
}

impl Tool {
    /// A tool `name` whose arguments `parameters`, a JSON schema, describes.
    pub fn new(name: impl Into<String>, parameters: Value) -> Self {
        Self {
            name: name.into(),
            description: None,
            parameters,
            native: None,
        }
    }
}

/// A tool the provider runs itself while it answers.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ProviderTool {
    /// A search of the web. What bounds it, such as its domains, is its
    /// format's `native`, which only that format sends.
    WebSearch {
        /// Its fields the kind has no place for.
        native: Option<NativeFields>,
    },
}

/// Whether, or which, tool the model must call.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ToolChoice {
    /// The model decides.
    Auto,
    /// No tool.
    None,
    /// Some tool.
    Required,
    /// The tool of this name.
    Tool(String),
}

impl From<&str> for Content {
    fn from(text: &str) -> Self {
        Content::Text {
            text: text.into(),
            native: None,
        }
    }
}

impl From<String> for Content {
    fn from(text: String) -> Self {
        Content::Text { text, native: None }
    }
}

/// The kind's output: one event of a reply, streamed or not (an `Output` is
/// not a whole reply; [`Assembled`] is the whole reply, folded from its
/// events). Filters see every event, those that are no content ([`Output::is_content`]) included,
/// such as a usage report.
///
/// Blocks are numbered by the format that decodes them, and may be open
/// together; an event names its block.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Output {
    /// The response begins: its id and model.
    Start(Start),
    /// Content block `block` opens.
    BlockStart {
        /// The block: a number unique in the response, which a format
        /// gives in the order blocks open, and a plugin may give its own.
        block: u32,
        /// What it holds.
        kind: Block,
    },
    /// The next part of block `block`, of the block's own kind.
    Delta {
        /// The block it belongs to.
        block: u32,
        /// The part, which says what it holds.
        delta: Delta,
    },
    /// Block `block` is complete.
    BlockEnd {
        /// The block that is complete.
        block: u32,
    },
    /// Usage the provider reports, as an increment. A plugin that meters a
    /// call sums it in a provider-edge filter, which sees every event.
    Usage(Usage),
    /// Why the model stopped.
    Finish(Finish),
    /// An event the kind has no form for, as `format` has it (pings,
    /// signatures, provider-specific events). Sent only in that format.
    Native(NativeValue),
}

/// Which response the events belong to.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Start {
    /// The response's id.
    pub id: String,
    /// The model that replied.
    pub model: String,
    /// The response's fields the kind has no place for, such as a
    /// fingerprint.
    pub native: Option<NativeFields>,
}

/// What a content block holds.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Block {
    /// Text.
    Text,
    /// A tool call the model makes; its [`Delta::Arguments`] are its
    /// arguments, as JSON text.
    ToolCall {
        /// The call's id, which its result names.
        id: String,
        /// The tool's name.
        name: String,
    },
    /// The model's reasoning.
    Reasoning,
}

/// The next part of a block, of the block's own kind.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Delta {
    /// Text of a [`Block::Text`].
    Text(String),
    /// The next part of a [`Block::ToolCall`]'s arguments, as JSON text.
    Arguments(String),
    /// Reasoning of a [`Block::Reasoning`].
    Reasoning(String),
}

impl Delta {
    /// The text it holds, whatever its kind.
    pub fn as_str(&self) -> &str {
        match self {
            Delta::Text(s) | Delta::Arguments(s) | Delta::Reasoning(s) => s,
        }
    }

    /// The text it holds, to append to.
    pub fn as_mut_string(&mut self) -> &mut String {
        match self {
            Delta::Text(s) | Delta::Arguments(s) | Delta::Reasoning(s) => s,
        }
    }
}

impl Start {
    /// The start of a response of this `id` and `model`.
    pub fn new(id: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            model: model.into(),
            native: None,
        }
    }
}

impl Output {
    /// The start of a response of this `id` and `model`.
    pub fn start(id: impl Into<String>, model: impl Into<String>) -> Self {
        Output::Start(Start::new(id, model))
    }

    /// Block `block` opens as `kind`.
    pub fn block_start(block: u32, kind: Block) -> Self {
        Output::BlockStart { block, kind }
    }

    /// The next text of the [`Block::Text`] `block`.
    pub fn text(block: u32, text: impl Into<String>) -> Self {
        Output::Delta {
            block,
            delta: Delta::Text(text.into()),
        }
    }

    /// The next part of the arguments of the [`Block::ToolCall`] `block`.
    pub fn arguments(block: u32, json: impl Into<String>) -> Self {
        Output::Delta {
            block,
            delta: Delta::Arguments(json.into()),
        }
    }

    /// The next reasoning of the [`Block::Reasoning`] `block`.
    pub fn reasoning(block: u32, text: impl Into<String>) -> Self {
        Output::Delta {
            block,
            delta: Delta::Reasoning(text.into()),
        }
    }

    /// Block `block` is complete.
    pub fn block_end(block: u32) -> Self {
        Output::BlockEnd { block }
    }

    /// Usage the provider reports, as an increment.
    pub fn usage(usage: Usage) -> Self {
        Output::Usage(usage)
    }

    /// The model stopped for this reason.
    pub fn finished(finish: Finish) -> Self {
        Output::Finish(finish)
    }

    /// An event of `format` the kind has no form for.
    pub fn native(format: impl Into<FormatId>, raw: impl Into<Value>) -> Self {
        Output::Native(NativeValue::new(format, raw))
    }

    /// Fields of block `block` the kind has no place for, as `format` has
    /// them, such as a signature: a client of `format` gets them back with
    /// the block, [`Assembled::replay`] keeps them on its part, and another
    /// format drops them. Read by [`block_fields`].
    pub fn block_fields(
        format: impl Into<FormatId>,
        block: u32,
        fields: Map<String, Value>,
    ) -> Self {
        Self::native(
            format,
            json!({ "type": BLOCK_FIELDS, "block": block, "fields": fields }),
        )
    }

    /// Whether it is content: a start, a block event or a finish. Usage and
    /// [`Output::Native`] are not.
    pub fn is_content(&self) -> bool {
        !matches!(
            self,
            Output::Usage(_) | Output::Native(respan_gateway_core::NativeValue { .. })
        )
    }
}

/// Why the model stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Finish {
    /// A natural end or a stop sequence.
    Stop,
    /// The token limit.
    Length,
    /// The model wants a tool called.
    ToolUse,
    /// A safety filter.
    Filtered,
    /// A reason the kind has no form for, as `format` names it; only a
    /// response in that format can say it, and another writes its
    /// `fallback`, if it has one.
    #[non_exhaustive]
    Native {
        /// The format that names it.
        format: FormatId,
        /// The reason, as that format names it.
        reason: String,
        /// The kind's reason nearest to it, which another format writes.
        fallback: Option<Box<Finish>>,
    },
}

impl Finish {
    /// The reason `reason` of `format`, which the kind has no form for.
    pub fn native(format: impl Into<FormatId>, reason: impl Into<String>) -> Self {
        Finish::Native {
            format: format.into(),
            reason: reason.into(),
            fallback: None,
        }
    }

    /// The reason `reason` of `format`, which another format writes as
    /// `fallback`.
    pub fn native_or(
        format: impl Into<FormatId>,
        reason: impl Into<String>,
        fallback: Finish,
    ) -> Self {
        Finish::Native {
            format: format.into(),
            reason: reason.into(),
            fallback: Some(Box::new(fallback)),
        }
    }
}

/// A response block with its deltas joined. Its text is ordinary text,
/// JSON argument text, or reasoning, according to `kind`.
#[derive(Debug, Clone, PartialEq)]
pub struct AssembledBlock {
    /// The block's identifier in the event stream.
    pub block: u32,
    /// What the block holds.
    pub kind: Block,
    /// The concatenated deltas, without parsing or rewriting them.
    pub text: String,
    ended: bool,
}

/// A block or opaque event in an assembled response. Blocks occupy the
/// position of their first start; opaque events retain their arrival order.
#[derive(Debug, Clone, PartialEq)]
pub enum AssembledItem {
    /// An assembled content block.
    Block(AssembledBlock),
    /// A format-specific event, kept without interpretation.
    Native(NativeValue),
}

/// Response events collected for inspection or whole-body encoding.
/// Text, tool calls, reasoning and opaque events stay in one ordered list.
/// This is a fold, not a check that the source completed: stream termination
/// and truncation belong to its reader. [`Assembled::replay`] reads it as the
/// assistant message that holds what was said.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Assembled {
    /// The response's start, if one came.
    pub start: Option<Start>,
    /// Why the model stopped, if it said.
    pub finish: Option<Finish>,
    /// All reported usage increments, summed.
    pub usage: Usage,
    items: Vec<AssembledItem>,
}

impl Assembled {
    /// The blocks and opaque events, in order. Read-only while assembling
    /// so callers cannot invalidate the state used by subsequent deltas.
    pub fn items(&self) -> &[AssembledItem] {
        &self.items
    }

    /// The content blocks, in the order they opened.
    pub fn blocks(&self) -> impl Iterator<Item = &AssembledBlock> {
        self.items.iter().filter_map(|item| match item {
            AssembledItem::Block(part) => Some(part),
            AssembledItem::Native(..) => None,
        })
    }

    /// Join text blocks, skipping tool arguments, reasoning and opaque events.
    pub fn text(&self) -> String {
        self.blocks()
            .filter(|part| part.kind == Block::Text)
            .map(|part| part.text.as_str())
            .collect()
    }

    /// What the model said, as the assistant message holds it: its text,
    /// reasoning and tool calls in order, each with the fields its format kept
    /// beside it.
    pub fn replay(&self) -> Vec<Content> {
        let mut kept: HashMap<u32, NativeFields> = HashMap::new();
        for item in self.items() {
            let AssembledItem::Native(native) = item else {
                continue;
            };
            let Some((block, fields)) = block_fields(&native.value) else {
                continue;
            };
            let entry = kept
                .entry(block)
                .or_insert_with(|| NativeFields::new(native.format.clone(), Map::new()));
            entry.fields.extend(fields.clone());
        }
        let native_of = |block: u32| kept.get(&block).filter(|n| !n.is_empty()).cloned();
        let mut parts = Vec::new();
        for item in self.items() {
            match item {
                AssembledItem::Block(b) => {
                    let native = native_of(b.block);
                    match &b.kind {
                        Block::Text if !b.text.is_empty() => {
                            let mut part = Content::from(b.text.clone());
                            if let Content::Text { native: n, .. } = &mut part {
                                *n = native;
                            }
                            parts.push(part);
                        }
                        Block::Reasoning if !b.text.is_empty() || native.is_some() => {
                            parts.push(Content::Reasoning {
                                text: b.text.clone(),
                                native,
                            });
                        }
                        Block::ToolCall { id, name } => {
                            let arguments = Some(b.text.as_str()).filter(|t| !t.trim().is_empty());
                            let mut part = Content::tool_call(id, name, arguments.unwrap_or("{}"));
                            if let Content::ToolCall { native: n, .. } = &mut part {
                                *n = native;
                            }
                            parts.push(part);
                        }
                        _ => {}
                    }
                }
                // Thinking the provider redacted is only a native block; it must
                // be sent back as it came.
                AssembledItem::Native(native) => {
                    let block = &native.value["content_block"];
                    if native.value["type"] == "content_block_start"
                        && block["type"] == "redacted_thinking"
                    {
                        parts.push(Content::native(native.format.clone(), block.clone()));
                    }
                }
            }
        }
        parts
    }

    /// Fold the next event. Reject duplicate response starts, conflicting
    /// block starts, mismatched deltas, and events for unopened or closed
    /// blocks. Repeating the same start of an open block is allowed, so a
    /// filter can append to it without opening another block.
    pub fn push(&mut self, reply: &Output) -> Result<(), Fault> {
        match reply {
            Output::Start(start) => match self.start {
                Some(_) => return Err(malformed("the response starts twice")),
                None => self.start = Some(start.clone()),
            },
            Output::BlockStart { block, kind } => match self.part(*block) {
                Some(part) if part.kind == *kind && !part.ended => {}
                Some(_) => return Err(malformed("a block opens twice")),
                None => self.items.push(AssembledItem::Block(AssembledBlock {
                    block: *block,
                    kind: kind.clone(),
                    text: String::new(),
                    ended: false,
                })),
            },
            Output::Delta { block, delta } => {
                let part = self.open_part(*block)?;
                if !matches!(
                    (&part.kind, delta),
                    (Block::Text, Delta::Text(_))
                        | (Block::ToolCall { .. }, Delta::Arguments(_))
                        | (Block::Reasoning, Delta::Reasoning(_))
                ) {
                    return Err(malformed("a delta is not of its block's kind"));
                }
                part.text.push_str(delta.as_str());
            }
            Output::BlockEnd { block } => self.open_part(*block)?.ended = true,
            Output::Usage(usage) => self.usage += usage,
            Output::Finish(finish) => self.finish = Some(finish.clone()),
            Output::Native(respan_gateway_core::NativeValue { format, value: raw }) => {
                self.items.push(AssembledItem::Native(NativeValue {
                    format: format.clone(),
                    value: raw.clone(),
                }));
            }
        }
        Ok(())
    }

    fn part(&mut self, block: u32) -> Option<&mut AssembledBlock> {
        self.items.iter_mut().find_map(|item| match item {
            AssembledItem::Block(part) if part.block == block => Some(part),
            _ => None,
        })
    }

    fn open_part(&mut self, block: u32) -> Result<&mut AssembledBlock, Fault> {
        match self.part(block) {
            Some(part) if !part.ended => Ok(part),
            Some(_) => Err(malformed("an event names a block that has ended")),
            None => Err(malformed("an event names a block that never opened")),
        }
    }
}

/// A sequence of events that is no response.
fn malformed(what: &str) -> Fault {
    Fault::upstream(format!("the response is malformed: {what}"))
}

/// The `type` of an [`Output::block_fields`] native.
const BLOCK_FIELDS: &str = "block_fields";

/// The block and fields of `raw`, if it is an [`Output::block_fields`]
/// native's.
pub fn block_fields(raw: &Value) -> Option<(u32, &Map<String, Value>)> {
    if raw["type"] != BLOCK_FIELDS {
        return None;
    }
    let block = u32::try_from(raw["block"].as_u64()?).ok()?;
    Some((block, raw["fields"].as_object()?))
}

/// The block a native event names, by its `block`, or by `index` as a
/// format's own block events name it; for a plugin that renumbers blocks.
pub fn native_block(raw: &Value) -> Option<u32> {
    let named = raw["index"].as_u64().or_else(|| raw["block"].as_u64());
    u32::try_from(named?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_core::FaultKind;
    use serde_json::json;

    fn fold(events: &[Output]) -> Result<Assembled, Fault> {
        let mut all = Assembled::default();
        events.iter().try_for_each(|e| all.push(e))?;
        Ok(all)
    }

    #[test]
    fn plain_values_compare_by_what_they_say() {
        let a = Message::new(Role::User, "hi");
        let mut b = a.clone();
        assert_eq!(a, b);
        b.native = Some(NativeFields::new(
            "chat",
            json!({"name": "x"})
                .as_object()
                .cloned()
                .unwrap_or_default(),
        ));
        assert_ne!(a, b);
        assert!(a.is_plain() && !b.is_plain());
        let tagged = Message::new(Role::native("chat", "tool"), "x");
        assert!(!tagged.is_plain());
    }

    #[test]
    fn parts_roles_and_finishes_carry_their_format_and_extra() {
        let marker = NativeFields::new(
            "chat",
            json!({"cache_control": 1})
                .as_object()
                .cloned()
                .unwrap_or_default(),
        );
        let mut cached = Message::new(Role::User, "hi");
        if let Content::Text { native, .. } = &mut cached.content[0] {
            *native = Some(marker);
        }
        // A part's native does not change what it says.
        assert!(cached.is_plain());
        assert_ne!(cached, Message::new(Role::User, "hi"));
        assert_eq!(
            Role::native("chat", "critic"),
            Role::native("chat", "critic")
        );
        assert_ne!(
            Role::native("chat", "critic"),
            Role::native("other", "critic")
        );
        assert_ne!(Finish::native("a", "x"), Finish::native("b", "x"));
    }

    #[test]
    fn events_fold_into_the_whole_answer() {
        let usage = Usage::new(3, 0);
        let all = fold(&[
            Output::start("r1", "m"),
            Output::block_start(0, Block::Reasoning),
            Output::reasoning(0, "hm"),
            Output::block_end(0),
            Output::block_start(1, Block::Text),
            Output::text(1, "Hel"),
            Output::text(1, "lo"),
            Output::block_end(1),
            Output::block_start(
                2,
                Block::ToolCall {
                    id: "t".into(),
                    name: "f".into(),
                },
            ),
            Output::arguments(2, "{\"a\":"),
            Output::arguments(2, "1}"),
            Output::block_end(2),
            Output::native("chat", json!({"type": "ping"})),
            Output::usage(usage),
            Output::usage(usage),
            Output::finished(Finish::ToolUse),
        ])
        .expect("a response");
        assert_eq!(all.start.as_ref().map(|s| s.id.as_str()), Some("r1"));
        let blocks: Vec<_> = all.blocks().map(|p| (&p.kind, p.text.as_str())).collect();
        assert_eq!(
            blocks,
            [
                (&Block::Reasoning, "hm"),
                (&Block::Text, "Hello"),
                (
                    &Block::ToolCall {
                        id: "t".into(),
                        name: "f".into()
                    },
                    "{\"a\":1}"
                ),
            ]
        );
        assert_eq!(all.text(), "Hello");
        assert_eq!(all.finish, Some(Finish::ToolUse));
        assert_eq!(all.usage.input_tokens, 6);
        assert!(
            matches!(all.items().last(), Some(AssembledItem::Native(respan_gateway_core::NativeValue { format: f, value: raw }))
            if &**f == "chat" && **raw == json!({"type": "ping"}))
        );
    }

    #[test]
    fn a_sequence_that_is_no_response_is_refused() {
        let text = || Output::block_start(0, Block::Text);
        for events in [
            vec![Output::text(0, "x")],
            vec![Output::block_end(0)],
            vec![text(), Output::block_end(0), Output::text(0, "x")],
            vec![text(), Output::block_end(0), Output::block_end(0)],
            vec![text(), Output::block_end(0), text()],
            vec![text(), Output::block_start(0, Block::Reasoning)],
            vec![text(), Output::reasoning(0, "x")],
            vec![text(), Output::arguments(0, "{}")],
            vec![
                Output::block_start(0, Block::Reasoning),
                Output::text(0, "x"),
            ],
            vec![Output::start("a", "m"), Output::start("b", "m")],
        ] {
            let end = fold(&events).err();
            assert!(end.is_some_and(|f| f.is(FaultKind::Provider)), "{events:?}");
        }
        // A start repeated for an open block, as a filter adding to it writes.
        assert!(fold(&[text(), Output::text(0, "a"), text(), Output::text(0, "b")]).is_ok());
    }

    #[test]
    fn interleaved_blocks_keep_their_place_beside_opaque_events() {
        let signature = json!({"index": 9, "signature": "signed"});
        let mut all = fold(&[
            Output::block_start(9, Block::Reasoning),
            Output::reasoning(9, "think"),
            Output::native("native", signature.clone()),
            Output::block_start(2, Block::Text),
            Output::text(2, "ans"),
            Output::reasoning(9, "ing"),
            Output::block_end(9),
            Output::text(2, "wer"),
        ])
        .expect("interleaved response");
        assert!(matches!(&all.items()[0], AssembledItem::Block(p)
            if p.block == 9 && p.kind == Block::Reasoning && p.text == "thinking"));
        assert!(
            matches!(&all.items()[1], AssembledItem::Native(respan_gateway_core::NativeValue { format: f, value: raw })
            if &**f == "native" && **raw == signature)
        );
        assert!(matches!(&all.items()[2], AssembledItem::Block(p)
            if p.block == 2 && p.kind == Block::Text && p.text == "answer"));
        assert_eq!(all.text(), "answer");

        let before = all.clone();
        assert!(all.push(&Output::arguments(2, "wrong kind")).is_err());
        assert_eq!(all, before, "a rejected delta changes nothing");
        all.push(&Output::text(2, "!")).expect("still open");
        all.push(&Output::block_end(2)).expect("closes");
        assert_eq!(all.text(), "answer!");
    }

    #[test]
    fn a_delta_of_another_kind_than_its_block_is_refused() {
        let call = Block::ToolCall {
            id: "t".into(),
            name: "f".into(),
        };
        for (kind, wrong) in [
            (Block::Text, Output::arguments(0, "{}")),
            (Block::Text, Output::reasoning(0, "x")),
            (call.clone(), Output::text(0, "x")),
            (call, Output::reasoning(0, "x")),
            (Block::Reasoning, Output::text(0, "x")),
            (Block::Reasoning, Output::arguments(0, "{}")),
        ] {
            let mut all = Assembled::default();
            all.push(&Output::block_start(0, kind.clone()))
                .expect("opens");
            let end = all.push(&wrong).err();
            assert!(
                end.is_some_and(|f| f.is(FaultKind::Provider)),
                "{kind:?} {wrong:?}"
            );
        }
    }
    #[test]
    fn only_usage_and_other_events_are_no_content() {
        assert!(Output::start("a", "m").is_content());
        assert!(Output::text(0, "x").is_content());
        assert!(Output::finished(Finish::Stop).is_content());
        assert!(!Output::usage(Usage::default()).is_content());
        assert!(!Output::native("f", json!(1)).is_content());
    }
}
