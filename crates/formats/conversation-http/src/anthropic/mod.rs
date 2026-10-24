//! Anthropic Messages: `POST /v1/messages`, JSON or SSE, of the
//! conversation kind, as a [`Schema`].
//!
//! # Responses as events
//!
//! A stream and a whole body decode into the same events, by these rules:
//!
//! - [`Output::Start`] comes from `message_start`, or the body's `id` and
//!   `model`; its `native` holds the message's other fields.
//! - Blocks are numbered by their `index`. A `text`, `tool_use` or
//!   `thinking` block opens a [`Block::Text`], [`Block::ToolCall`] or
//!   [`Block::Reasoning`]; `text_delta`, `input_json_delta` and
//!   `thinking_delta` are its deltas, and `content_block_stop` its end. A
//!   whole body's blocks are the same events, one delta each.
//! - `message_delta`'s `stop_reason` is the [`Output::Finish`]; `usage` of
//!   `message_start` and `message_delta` are [`Output::Usage`] increments.
//! - A block's fields the kind has no place for, its `signature` (a
//!   `signature_delta`) among them, are its [`Output::block_fields`].
//! - What else the kind has no form for is a [`Output::Native`] of this
//!   format, as the stream event that carries it: `ping`, a block
//!   of another type (and its deltas and stop), citations, a `stop_sequence`
//!   (as a `message_delta`), an event of an unknown type. In a whole body a
//!   block of another type is a `content_block_start` of the whole block.
//!   `message_stop` ends the stream.
//!
//! Encoders do the reverse. A stream's blocks are written one at a time, as
//! Anthropic does: a block that opens while another is open waits, with its
//! deltas, until that one ends. A whole body is written from the events
//! collected, empty text blocks left out, as Anthropic refuses them.

mod decode;
mod encode;
mod request;
#[cfg(test)]
mod tests;

use crate::is_inert;
use crate::schema::{
    Asked, Names, NativeFieldsExt, Schema, UsageFields, arguments_object, check_usage, count,
    decode_content, decode_message, encode_content, encode_message, merge_details, merge_fields,
    merge_into, native_fields, own_fields, says, set, stop_json, unsupported_fields,
    unwritable_content, unwritable_native, usage_extra,
};
use respan_gateway_conversation::{
    Assembled, AssembledItem, Block, Content, Finish, Input, Message, Output, Role, Usage,
    block_fields,
};
use respan_gateway_core::Provider;
use respan_gateway_core::{Connection, Fault, Format, NativeValue};
use respan_gateway_http::{ClientIo, Encoder, ProviderIo, http};
use respan_gateway_http::{Connect, KeyIn, Upstream};
use serde_json::{Map, Value, json};

use crate::openai::OPENAI;
use crate::responses::RESPONSES;
use decode::AnthropicDecoder;
use encode::EventStream;
use request::{
    read_effort, read_output, read_thinking, read_tools_of, write_output, write_thinking,
    write_tools,
};

/// The `max_tokens` of a translated request without one, as it is required.
const DEFAULT_MAX_TOKENS: u64 = 4096;

const FINISH: Names<Finish> = Names(
    &[
        ("end_turn", Finish::Stop),
        ("stop_sequence", Finish::Stop),
        ("max_tokens", Finish::Length),
        ("tool_use", Finish::ToolUse),
        ("refusal", Finish::Filtered),
    ],
    ANTHROPIC,
);

/// A reason as a [`Finish`]: a full context window is the token limit to
/// another format.
fn finish_of(reason: &str) -> Finish {
    match reason {
        "model_context_window_exceeded" => Finish::native_or(ANTHROPIC, reason, Finish::Length),
        _ => FINISH.finish(reason),
    }
}

/// This format's name.
const ANTHROPIC: &str = "anthropic-messages";

/// Anthropic Messages, named `anthropic-messages`: a `POST` to any path
/// ending in `/v1/messages`. The provider's base URL leaves out the
/// version (`https://api.anthropic.com`); `anthropic-version: 2023-06-01`
/// is sent if none is set.
#[derive(Debug, Clone, Copy, Default)]
pub struct AnthropicMessages;

impl AnthropicMessages {
    /// This format's name, `anthropic-messages`.
    pub fn name(&self) -> &'static str {
        ANTHROPIC
    }
}

impl Format<ClientIo, Input, Output> for AnthropicMessages {
    fn name(&self) -> &str {
        ANTHROPIC
    }
    fn matches(&self, io: &ClientIo) -> bool {
        self.accepts(io.request())
    }
    fn bind(&self, io: ClientIo) -> Result<Connection<Input, Output>, Fault> {
        self.bind_client(io)
    }
}

/// Its spec's: the key in `x-api-key`, over HTTP.
impl Upstream for AnthropicMessages {
    fn name(&self) -> &str {
        ANTHROPIC
    }

    fn key(&self) -> KeyIn {
        KeyIn::Header("x-api-key")
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for AnthropicMessages {
    fn name(&self) -> &str {
        ANTHROPIC
    }
    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        self.bind_provider(io)
    }
}

/// Keys a conversation has a place for, or that change no output.
const KNOWN: &[&str] = &[
    "model",
    "messages",
    "system",
    "max_tokens",
    "temperature",
    "top_p",
    "top_k",
    "stop_sequences",
    "stream",
    "metadata",
];

/// Roles a message can have; system prompts have their own field.
const ROLES: Names<Role> = Names(
    &[("user", Role::User), ("assistant", Role::Assistant)],
    ANTHROPIC,
);

/// Whether a message is instructions: Anthropic has one system prompt for a
/// system's and a developer's alike.
fn is_system(m: &Message) -> bool {
    matches!(m.role, Role::System | Role::Developer)
}

/// The parts of the leading system messages, as the system prompt, and
/// the rest; a later system message is refused.
fn split_system(chat: &Input) -> Result<(Vec<Content>, &[Message]), Fault> {
    let lead = chat.messages.iter().take_while(|m| is_system(m)).count();
    let (system, rest) = chat.messages.split_at(lead);
    if rest.iter().any(is_system) {
        return Err(Fault::bad_request(
            "Anthropic has no place for a system message after the conversation starts",
        ));
    }
    if system.iter().any(|m| !m.native.is_empty()) {
        return Err(Fault::bad_request(
            "Anthropic's system prompt has no place for a message's other fields",
        ));
    }
    let parts = system.iter().flat_map(|m| m.content.iter().cloned());
    Ok((parts.collect(), rest))
}

/// `m` as Anthropic takes it: without reasoning it did not sign, which it
/// refuses and lets an earlier turn leave out, and without empty text parts
/// if it says something else, such as a tool call.
fn writable(m: &Message) -> std::borrow::Cow<'_, Message> {
    let unsigned = |c: &Content| match c {
        Content::Reasoning { native, .. } => {
            native.format() != Some(ANTHROPIC) || !native.fields().contains_key("signature")
        }
        _ => false,
    };
    let mut m = std::borrow::Cow::Borrowed(m);
    if m.content.iter().any(unsigned) {
        m.to_mut().content.retain(|c| !unsigned(c));
    }
    let empty = |c: &Content| matches!(c, Content::Text { text, .. } if text.is_empty());
    if m.content.iter().any(empty) && !m.content.iter().all(empty) {
        m.to_mut().content.retain(|c| !empty(c));
    }
    m
}

/// Whether a message says nothing: no content, or only empty text.
fn is_empty(m: &Message) -> bool {
    let empty = |c: &Content| matches!(c, Content::Text { text, .. } if text.is_empty());
    m.native.is_empty() && m.content.iter().all(empty)
}

/// Anthropic counts no reasoning apart from the output.
const USAGE: UsageFields = UsageFields {
    cache_writes: true,
    reasoning: false,
};

impl Schema for AnthropicMessages {
    const NAME: &'static str = ANTHROPIC;
    const PATH: &'static str = "/v1/messages";
    /// The API version sent when the client names none.
    const HEADERS: &'static [(&'static str, &'static str)] = &[("anthropic-version", "2023-06-01")];
    const CARRIED: &'static [&'static str] = &["anthropic-version", "anthropic-beta"];
    const CARRIED_QUERY: &'static [&'static str] = &["beta"];
    const OWN: &'static [&'static str] = &["metadata", "output_config", "thinking"];
    /// Messages' own fields an OpenAI client may send for an Anthropic
    /// model, from either OpenAI format: where it is served
    /// (`inference_geo`), its capacity (`service_tier`) and `speed`, the
    /// sampling cut-off (`top_k`), the cache breakpoint (`cache_control`),
    /// and the end user's id for abuse checks (`safety_identifier`, as
    /// `metadata.user_id`).
    const ADOPTED: &'static [(&'static str, &'static str, &'static str)] = &[
        (OPENAI, "inference_geo", "inference_geo"),
        (OPENAI, "service_tier", "service_tier"),
        (OPENAI, "speed", "speed"),
        (OPENAI, "top_k", "top_k"),
        (OPENAI, "cache_control", "cache_control"),
        (OPENAI, "safety_identifier", "metadata.user_id"),
        (RESPONSES, "inference_geo", "inference_geo"),
        (RESPONSES, "service_tier", "service_tier"),
        (RESPONSES, "speed", "speed"),
        (RESPONSES, "top_k", "top_k"),
        (RESPONSES, "cache_control", "cache_control"),
        (RESPONSES, "safety_identifier", "metadata.user_id"),
    ];
    type Decoding = AnthropicDecoder;

    fn decoder(&self) -> AnthropicDecoder {
        AnthropicDecoder::default()
    }

    fn decode_body(&self, v: &Value, out: &mut Vec<Output>) -> Result<(), Fault> {
        self.decoder().body(v, out)
    }

    fn decode_part(p: &Value) -> Option<Content> {
        request::decode_part(p)
    }

    fn encode_part(c: &Content) -> Result<Value, Fault> {
        request::encode_part(c)
    }

    fn stream_encoder(&self, _asked: Asked) -> Box<dyn Encoder<Output>> {
        Box::new(EventStream::default())
    }

    /// Anthropic's 529 (`overloaded_error`) says what HTTP's 503 does: the
    /// service is overloaded, try later. So it reads as a 503, which retry
    /// ([`retryable`](respan_gateway_http_faults::retryable)), routing and a client
    /// of any format understand; the answer as it came, 529, stays for a
    /// relay of its error body
    /// ([`ProviderError`](respan_gateway_http_faults::ProviderError)).
    fn status(status: http::StatusCode) -> http::StatusCode {
        match status.as_u16() {
            529 => http::StatusCode::SERVICE_UNAVAILABLE,
            _ => status,
        }
    }

    /// A ping, or what is kept beside a response: a signature, a stop
    /// sequence, usage fields with no unit, a tool call's fields.
    fn is_inert(raw: &Value) -> bool {
        matches!(raw["type"].as_str(), Some("ping" | "message_delta"))
    }

    /// The system prompt becomes a leading system message.
    fn decode_json(&self, v: &Value) -> Input {
        let mut chat = Input::default();
        chat.options.max_output_tokens = v["max_tokens"].as_u64();
        chat.options.temperature = v["temperature"].as_f64();
        chat.options.top_p = v["top_p"].as_f64();
        chat.options.top_k = v["top_k"].as_u64();
        chat.options.stop = v["stop_sequences"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| s.as_str().map(Into::into))
            .collect();
        chat.user = v["metadata"]["user_id"].as_str().map(Into::into);
        chat.native = native_fields(ANTHROPIC, unsupported_fields(v, KNOWN, &[]));
        // Metadata beside the user id is kept for this format to write again.
        let others = v["metadata"].as_object().into_iter().flatten();
        let others: Map<String, Value> = others
            .filter(|(k, x)| *k != "user_id" && says(x))
            .map(|(k, x)| (k.clone(), x.clone()))
            .collect();
        if !others.is_empty() {
            chat.native
                .fields_mut(ANTHROPIC)
                .insert("metadata".into(), others.into());
        }
        if says(&v["system"]) {
            let mut system = Message::new(Role::System, "");
            system.content = decode_content::<Self>(&v["system"]);
            chat.messages.push(system);
        }
        for m in v["messages"].as_array().into_iter().flatten() {
            chat.messages.push(decode_message::<Self>(m, &ROLES));
        }
        read_tools_of(v, &mut chat);
        chat.native = native_fields(ANTHROPIC, std::mem::take(chat.native.fields_mut(ANTHROPIC)));
        read_output(v, &mut chat);
        read_thinking(&mut chat);
        read_effort(&mut chat);
        chat
    }

    fn encode_json(&self, chat: &Input) -> Result<Map<String, Value>, Fault> {
        let (system, rest) = split_system(chat)?;
        if rest.is_empty() {
            return Err(Fault::bad_request(
                "Anthropic needs at least one user or assistant message",
            ));
        }
        if chat.options.temperature.is_some_and(|t| t > 1.0) {
            return Err(Fault::bad_request(
                "Anthropic's temperature is at most 1, so a higher one cannot be translated",
            ));
        }
        let thinking = write_thinking(chat)?;
        // Anthropic's `max_tokens` must exceed the thinking budget.
        let budget = thinking.as_ref().and_then(|t| t["budget_tokens"].as_u64());
        let default = match budget {
            Some(budget) if budget >= DEFAULT_MAX_TOKENS => {
                budget.saturating_add(DEFAULT_MAX_TOKENS)
            }
            _ => DEFAULT_MAX_TOKENS,
        };
        let mut v = Map::new();
        let max_tokens = chat.options.max_output_tokens.unwrap_or(default);
        v.insert("max_tokens".into(), max_tokens.into());
        set(
            &mut v,
            "system",
            (!system.is_empty())
                .then(|| encode_content::<Self>(&system))
                .transpose()?,
        );
        // Anthropic refuses an empty turn but the last; dropping one leaves
        // consecutive turns of a role, which it combines.
        let last = rest.len().saturating_sub(1);
        let kept = rest
            .iter()
            .map(writable)
            .enumerate()
            .filter(|(i, m)| *i == last || !is_empty(m));
        let messages = kept.map(|(_, m)| encode_message::<Self>(&m, &ROLES));
        let messages: Vec<Value> = messages.collect::<Result<_, _>>()?;
        v.insert("messages".into(), messages.into());
        set(
            &mut v,
            "temperature",
            chat.options.temperature.map(Into::into),
        );
        set(&mut v, "top_p", chat.options.top_p.map(Into::into));
        set(&mut v, "top_k", chat.options.top_k.map(Into::into));
        set(&mut v, "stop_sequences", stop_json(&chat.options.stop));
        let mut metadata = Value::Object(Map::new());
        if let Some(user) = &chat.user {
            metadata["user_id"] = user.as_str().into();
        }
        let own = chat.native.format() == Some(ANTHROPIC);
        if let Some(more) = chat
            .native
            .fields()
            .get("metadata")
            .and_then(Value::as_object)
        {
            merge_fields(
                &mut metadata,
                &more
                    .iter()
                    .filter(|_| own)
                    .map(|(k, x)| (k.clone(), x.clone()))
                    .collect(),
            );
        }
        let empty = metadata.as_object().is_none_or(Map::is_empty);
        set(&mut v, "metadata", (!empty).then_some(metadata));
        write_tools(chat, &mut v)?;
        write_output(chat, &mut v)?;
        set(&mut v, "thinking", thinking);
        Ok(v)
    }

    fn is_response(v: &Value) -> bool {
        v["type"] == "message"
    }

    /// The blocks in the order they came, empty text left out.
    fn encode_whole(&self, whole: &Assembled) -> Result<Value, Fault> {
        check_usage(&whole.usage, USAGE)?;
        let mut content: Vec<Value> = Vec::new();
        // Where each block, by number, is in `content`, and the JSON text of
        // the input of an event-made block, which is parsed at the end.
        let mut at: Vec<(u32, usize)> = Vec::new();
        let mut input: Vec<(usize, String)> = Vec::new();
        // The fields of blocks that come next.
        let mut fields: Vec<(u32, Map<String, Value>)> = Vec::new();
        let mut stop = StopFields::default();
        for item in whole.items() {
            match item {
                AssembledItem::Block(part) => {
                    let block = match &part.kind {
                        Block::Text if part.text.is_empty() => continue,
                        Block::Text => json!({ "type": "text", "text": part.text }),
                        Block::Reasoning => json!({ "type": "thinking", "thinking": part.text }),
                        Block::ToolCall { id, name } => {
                            let input =
                                arguments_object(&part.text).map_err(|_| unwritable_content())?;
                            json!({ "type": "tool_use", "id": id, "name": name, "input": input })
                        }
                        _ => return Err(unwritable_content()),
                    };
                    let mut block = block;
                    if let Some(i) = fields.iter().position(|(b, _)| *b == part.block) {
                        merge_fields(&mut block, &fields.remove(i).1);
                    }
                    at.push((part.block, content.len()));
                    content.push(block);
                }
                AssembledItem::Native(NativeValue { format, value: raw })
                    if **format == *ANTHROPIC && block_fields(raw).is_some() =>
                {
                    let Some((block, more)) = block_fields(raw) else {
                        continue;
                    };
                    // A block placed takes them now, as a signature after it.
                    match at.iter().find(|(b, _)| *b == block) {
                        Some((_, place)) => merge_fields(&mut content[*place], more),
                        None => fields.push((block, more.clone())),
                    }
                }
                AssembledItem::Native(NativeValue { format, value: raw })
                    if **format == *ANTHROPIC =>
                {
                    let place = raw["index"]
                        .as_u64()
                        .and_then(|i| u32::try_from(i).ok())
                        .and_then(|i| at.iter().find(|(b, _)| *b == i))
                        .map(|(_, place)| *place);
                    match (raw["type"].as_str(), place) {
                        (Some("content_block_start"), _) => {
                            let index = raw["index"].as_u64().and_then(|i| u32::try_from(i).ok());
                            at.extend(index.map(|i| (i, content.len())));
                            content.push(raw["content_block"].clone());
                        }
                        (Some("content_block_stop" | "ping"), _) => {}
                        (Some("message_delta"), _) => stop.add(raw),
                        (Some("content_block_delta"), Some(place)) => {
                            let delta = &raw["delta"];
                            match delta["type"].as_str() {
                                Some("signature_delta") => {
                                    content[place]["signature"] = delta["signature"].clone();
                                }
                                Some("citations_delta") => {
                                    let cites = &mut content[place]["citations"];
                                    if !cites.is_array() {
                                        *cites = json!([]);
                                    }
                                    if let Some(cites) = cites.as_array_mut() {
                                        cites.push(delta["citation"].clone());
                                    }
                                }
                                Some("input_json_delta") => {
                                    let json = delta["partial_json"].as_str().unwrap_or_default();
                                    match input.iter_mut().find(|(p, _)| *p == place) {
                                        Some((_, text)) => text.push_str(json),
                                        None => input.push((place, json.to_owned())),
                                    }
                                }
                                _ => return Err(unwritable_content()),
                            }
                        }
                        _ => return Err(unwritable_content()),
                    }
                }
                AssembledItem::Native(NativeValue { format, value: raw })
                    if is_inert(format, raw) => {}
                AssembledItem::Native(NativeValue { format, value: raw }) => {
                    return Err(unwritable_native(format, raw));
                }
            }
        }
        for (place, text) in input {
            content[place]["input"] = arguments_object(&text).map_err(|_| unwritable_content())?;
        }
        let finish = whole.finish.as_ref().unwrap_or(&Finish::Stop);
        let (id, model) = whole
            .start
            .as_ref()
            .map(|s| (s.id.as_str(), s.model.as_str()))
            .unwrap_or_default();
        let mut body = Map::new();
        body.insert("id".into(), id.into());
        body.insert("type".into(), "message".into());
        body.insert("role".into(), "assistant".into());
        body.insert("model".into(), model.into());
        body.extend(own_fields(whole.start.as_ref(), ANTHROPIC));
        body.insert("content".into(), content.into());
        body.insert("stop_reason".into(), stop.reason(finish)?.into());
        let sequence = stop.delta.get("stop_sequence").cloned();
        body.insert("stop_sequence".into(), sequence.unwrap_or(Value::Null));
        body.insert("usage".into(), usage_body(&whole.usage, &stop.usage));
        body.extend(stop.top);
        Ok(body.into())
    }

    fn error_body(fault: &Fault) -> Value {
        let error = json!({ "type": fault.code(), "message": fault.message() });
        json!({ "type": "error", "error": merge_details(error, fault) })
    }
}

/// What a `message_delta` [`Output::Native`] says beyond the finish and usage:
/// fields of its `delta`, such as the stop sequence, and its own.
#[derive(Debug, Default)]
struct StopFields {
    delta: Map<String, Value>,
    top: Map<String, Value>,
    /// The usage fields that have no unit, such as the cache breakdown.
    usage: Map<String, Value>,
}

impl StopFields {
    /// The reason to write for `finish`: a stop that hit a sequence says so.
    fn reason<'a>(&self, finish: &'a Finish) -> Result<&'a str, Fault> {
        match (finish, self.delta.get("stop_sequence")) {
            (Finish::Stop, Some(Value::String(_))) => Ok("stop_sequence"),
            _ => FINISH.reason(finish),
        }
    }

    fn add(&mut self, raw: &Value) {
        for (key, value) in raw.as_object().into_iter().flatten() {
            match (key.as_str(), value.as_object()) {
                ("type", _) => {}
                ("usage", Some(usage)) => merge_into(&mut self.usage, usage),
                ("delta", Some(delta)) => self.delta.extend(delta.clone()),
                _ => _ = self.top.insert(key.clone(), value.clone()),
            }
        }
    }
}

/// Usage as this format reports it: the cache reads and writes beside the
/// input they are part of, and reasoning as `thinking_tokens`.
fn usage_body(u: &Usage, native: &Map<String, Value>) -> Value {
    let (cached, written) = (u.cached_input_tokens, u.cache_write_input_tokens);
    let mut body = json!({
        "input_tokens": u.input_tokens.saturating_sub(cached).saturating_sub(written),
        "output_tokens": u.output_tokens,
    });
    if cached > 0 {
        body["cache_read_input_tokens"] = json!(cached);
    }
    if written > 0 {
        body["cache_creation_input_tokens"] = json!(written);
    }
    if u.reasoning_tokens > 0 {
        body["output_tokens_details"] = json!({ "thinking_tokens": u.reasoning_tokens });
    }
    merge_fields(&mut body, native);
    body
}

/// What a usage object says that has no unit: the cache breakdown, the
/// service tier, and details beside the thinking count.
fn extra_usage(u: &Value) -> Map<String, Value> {
    let known = [
        "input_tokens",
        "output_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
    ];
    let details: [(&str, &[&str]); 1] = [("output_tokens_details", &["thinking_tokens"])];
    usage_extra(u, &known, &details)
}

/// Usage totals; [`Usage::input_tokens`] includes the cache reads and
/// writes Anthropic counts apart.
fn usage_of(u: &Value) -> Result<Usage, Fault> {
    let n = |key: &str| count(u.get(key));
    let mut usage = Usage::default();
    if let Some(input) = n("input_tokens")? {
        let cached = n("cache_read_input_tokens")?.unwrap_or(0);
        let written = n("cache_creation_input_tokens")?.unwrap_or(0);
        usage.input_tokens = input.saturating_add(cached).saturating_add(written);
        usage.cached_input_tokens = cached;
        usage.cache_write_input_tokens = written;
    }
    usage.output_tokens = n("output_tokens")?.unwrap_or(0);
    let thinking = u.pointer("/output_tokens_details/thinking_tokens");
    usage.reasoning_tokens = count(thinking)?.unwrap_or(0);
    Ok(usage)
}
