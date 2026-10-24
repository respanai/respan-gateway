//! OpenAI Chat Completions: `POST /chat/completions`, JSON or SSE, of the
//! conversation kind, as a [`Schema`] ([`OpenAiChat`]).
//!
//! # Responses as events
//!
//! A chunk of a stream and a whole body decode into the same events, by
//! these rules, which are deterministic:
//!
//! - [`Output::Start`] comes once, with the first chunk that names an `id`. Its
//!   `native` holds the response's other fields (`created`, `system_fingerprint`,
//!   `service_tier`, a vendor's own object, ...) as the chunk has them.
//! - Blocks are numbered in the order they open. The first choice's
//!   `reasoning_content`, `content` and each `tool_calls` entry (by its
//!   `index`, else by its `id`) open a [`Block::Reasoning`], a
//!   [`Block::Text`] and a [`Block::ToolCall`] on their first non-empty
//!   delta, or, for a call, on the entry that names its `id` and `name`.
//!   Opening any block ends the reasoning or text block that is open; tool
//!   calls stay open, as they interleave. `reasoning_content` is no field
//!   of OpenAI's own: it is where every OpenAI-compatible vendor that
//!   reasons says it (DeepSeek, vLLM, ...).
//! - A `finish_reason`, or the end of the stream, ends every block still
//!   open, in the order they opened, and a finish gives a [`Output::Finish`].
//! - `usage` gives a [`Output::Usage`] of what it adds to the totals
//!   reported so far.
//! - A data line that is no chunk is a `Native` of its text. `[DONE]` ends
//!   the stream.
//!
//! # What the kind has no form for
//!
//! What a response says beyond the spec, or that the kind has no form for,
//! is kept as this format's own (`openai-chat`), where it appeared, so a
//! plugin can read it and a client of this format gets it back:
//!
//! - The response's own fields: the first chunk's in the start's `native`;
//!   those a later chunk changes, after its usage, as one
//!   [`Output::Native`] `{"response": {<field>: <value>, ..}}`, which another
//!   format drops, as it drops the start's.
//! - A choice's: one [`Output::Native`] `{"choices": [..]}` before the
//!   chunk's blocks, each choice as it came but for what the kind took, the
//!   first one's message or delta under `delta`: the fields of the first
//!   choice (`logprobs`, a vendor's own) and of its body (`refusal`, content
//!   in parts, a vendor's own), and every other choice whole.
//! - Usage's fields beyond its counts: `{"usage": {..}}`, which another
//!   format drops.
//!
//! Encoders do the reverse. A stream is written as chunks: the opening
//! `role` chunk, a chunk per delta, and, at the end, the finish, the usage
//! chunk if the client asked for it, and `[DONE]`. A whole body is written
//! from the events collected: all text joined into the message `content`,
//! all reasoning into its `reasoning_content`. A block is any number unique
//! in the response: chunks number tool calls in the order they open. A call
//! writes no reasoning: OpenAI takes none back, and a vendor that does says
//! so in its dialect.

mod decode;
mod encode;
mod request;
#[cfg(test)]
mod tests;

use crate::schema::{
    Asked, Names, NativeFieldsExt, Schema, UsageFields, check_usage, count, decode_message,
    merge_details, merge_fields, merge_into, native_fields, parse_effort, set, stop_json,
    take_native, tidy, unix_now, unsupported_fields, untranslatable, usage_extra,
};
use respan_gateway_conversation::{
    Assembled, AssembledBlock, AssembledItem, Block, Content, Finish, Input, Output, Reasoning,
    Role, Usage,
};
use respan_gateway_core::Provider;
use respan_gateway_core::{Connection, Fault, Format, NativeValue};
use respan_gateway_http::{ClientIo, Encoder, ProviderIo};
use respan_gateway_http::{Connect, Upstream};
use serde_json::{Map, Value, json};

use decode::OpenAiDecoder;
use encode::{Chunks, call_fields_of, call_json, envelope, merge_call, merge_choice};
use request::{
    read_openai_message, read_openai_tools, read_response_format, write_openai_message,
    write_openai_tools, write_reasoning, write_response_format,
};

/// The wire format's name ([`Schema::NAME`]).
pub(crate) const OPENAI: &str = "openai-chat";

/// OpenAI Chat Completions, `openai-chat`: a `POST` to any path ending in
/// `/chat/completions`. The provider's base URL includes the version, such
/// as `https://api.openai.com/v1`. It reads OpenAI's fields; what else a
/// vendor sends stays native, as the module says.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenAiChat;

/// The counts of a usage object `u` of this format, and its fields beside
/// them; `Err` for a count that is no count.
pub fn read_usage(u: &Value) -> Result<(Usage, Map<String, Value>), Fault> {
    Ok((usage_of(u)?, extra_usage(u)))
}

/// JSON fields.
type Fields = Map<String, Value>;

const FINISH: Names<Finish> = Names(
    &[
        ("stop", Finish::Stop),
        ("length", Finish::Length),
        ("tool_calls", Finish::ToolUse),
        ("function_call", Finish::ToolUse),
        ("content_filter", Finish::Filtered),
    ],
    OPENAI,
);

impl Format<ClientIo, Input, Output> for OpenAiChat {
    fn name(&self) -> &str {
        OPENAI
    }
    fn matches(&self, io: &ClientIo) -> bool {
        self.accepts(io.request())
    }
    fn bind(&self, io: ClientIo) -> Result<Connection<Input, Output>, Fault> {
        self.bind_client(io)
    }
}

/// Its spec's: a bearer key, over HTTP, beside its base URL.
impl Upstream for OpenAiChat {
    fn name(&self) -> &str {
        OPENAI
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for OpenAiChat {
    fn name(&self) -> &str {
        OPENAI
    }
    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        self.bind_provider(io)
    }
}

/// Keys a conversation has a place for, or that change no output; `user`
/// too, when it is a string.
/// A request's fields only OpenAI acts on, which another provider drops.
const OPENAI_ONLY: &[&str] = &["store", "metadata", "prompt_cache_key", "safety_identifier"];

const KNOWN: &[&str] = &[
    "model",
    "messages",
    "stream",
    "stream_options",
    "max_tokens",
    "max_completion_tokens",
    "temperature",
    "top_p",
    "stop",
];

/// Keys that change nothing at these values, as SDKs send by default.
const INERT: &[(&str, f64)] = &[
    ("n", 1.0),
    ("presence_penalty", 0.0),
    ("frequency_penalty", 0.0),
    ("logprobs", 0.0),
];

const ROLES: Names<Role> = Names(
    &[
        ("system", Role::System),
        ("developer", Role::Developer),
        ("user", Role::User),
        ("assistant", Role::Assistant),
    ],
    OPENAI,
);

/// OpenAI Chat counts no cache writes of its own; another provider's, part
/// of the prompt's tokens, are written beside its cached ones as
/// `prompt_tokens_details.cache_write_tokens`, as Responses names them.
const USAGE: UsageFields = UsageFields {
    cache_writes: true,
    reasoning: true,
};

impl Schema for OpenAiChat {
    const NAME: &'static str = OPENAI;
    const PATH: &'static str = "/chat/completions";
    const CARRIED: &'static [&'static str] =
        &["openai-beta", "openai-organization", "openai-project"];
    /// The two spellings of a response's token limit, and the structured
    /// output requirement.
    const OWN: &'static [&'static str] = &[
        "max_tokens",
        "max_completion_tokens",
        "response_format",
        "reasoning_effort",
    ];
    type Decoding = OpenAiDecoder;

    fn decoder(&self) -> OpenAiDecoder {
        OpenAiDecoder::new()
    }

    fn decode_body(&self, v: &Value, out: &mut Vec<Output>) -> Result<(), Fault> {
        self.decoder().body(v, out)
    }

    fn decode_json(&self, v: &Value) -> Input {
        let mut chat = Input::default();
        // The larger of both spellings, so the limit read bounds what any
        // provider enforces.
        chat.options.max_output_tokens = v["max_completion_tokens"]
            .as_u64()
            .max(v["max_tokens"].as_u64());
        chat.options.temperature = v["temperature"].as_f64();
        chat.options.top_p = v["top_p"].as_f64();
        chat.options.stop = match &v["stop"] {
            Value::String(s) => vec![s.clone()],
            Value::Array(a) => a
                .iter()
                .filter_map(|s| s.as_str().map(Into::into))
                .collect(),
            _ => Vec::new(),
        };
        chat.user = v["user"].as_str().map(Into::into);
        chat.native = native_fields(OPENAI, unsupported_fields(v, KNOWN, INERT));
        if chat.user.is_some() {
            chat.native.fields_mut(OPENAI).shift_remove("user");
        }
        // OpenAI's own: whether it keeps the completion, its tags for it,
        // and hints for its cache and abuse checks. Another provider keeps
        // nothing and needs no hint, so drops them.
        for key in OPENAI_ONLY {
            if let Some(value) = chat.native.fields_mut(OPENAI).shift_remove(*key) {
                let fields = chat.native.fields_mut(OPENAI);
                let inert = fields
                    .entry(crate::schema::INERT)
                    .or_insert_with(|| json!({}));
                inert[*key] = value;
            }
        }
        for m in v["messages"].as_array().into_iter().flatten() {
            let mut message = decode_message::<Self>(m, &ROLES);
            read_openai_message(m, &mut message);
            tidy(&mut message);
            chat.messages.push(message);
        }
        read_openai_tools(v, &mut chat);
        chat.native = native_fields(OPENAI, std::mem::take(chat.native.fields_mut(OPENAI)));
        read_response_format(v, &mut chat);
        chat.options.reasoning =
            take_native(&mut chat, "reasoning_effort", |e| parse_effort(e.as_str()?))
                .map(Reasoning::effort);
        chat
    }

    fn encode_json(&self, chat: &Input) -> Result<Map<String, Value>, Fault> {
        // Several choices cannot be represented as events.
        let several = |n: &Value| n.as_u64().is_some_and(|n| n > 1);
        if chat.native.format() == Some(OPENAI)
            && chat.native.fields().get("n").is_some_and(several)
        {
            return Err(Fault::bad_request(
                "several choices (n > 1) cannot be represented",
            ));
        }
        let mut v = Map::new();
        let mut messages = Vec::new();
        for m in &chat.messages {
            messages.extend(write_openai_message(m, &ROLES)?);
        }
        v.insert("messages".into(), messages.into());
        write_openai_tools(chat, &mut v)?;
        set(
            &mut v,
            "max_completion_tokens",
            chat.options.max_output_tokens.map(Into::into),
        );
        set(
            &mut v,
            "temperature",
            chat.options.temperature.map(Into::into),
        );
        set(&mut v, "top_p", chat.options.top_p.map(Into::into));
        set(&mut v, "stop", stop_json(&chat.options.stop));
        set(&mut v, "user", chat.user.clone().map(Into::into));
        set(&mut v, "response_format", write_response_format(chat));
        set(&mut v, "reasoning_effort", write_reasoning(chat)?);
        if chat.options.top_k.is_some() {
            return Err(untranslatable("top_k in OpenAI Chat"));
        }
        Ok(v)
    }

    fn decode_part(p: &Value) -> Option<Content> {
        request::decode_part(p)
    }

    fn encode_part(c: &Content) -> Result<Value, Fault> {
        request::encode_part(c)
    }

    fn is_response(v: &Value) -> bool {
        v["choices"].is_array()
    }

    /// One text of every block, then the tool calls, as one choice; other
    /// events as the choices they came in.
    fn encode_whole(&self, whole: &Assembled) -> Result<Value, Fault> {
        check_usage(&whole.usage, USAGE)?;
        let join = |kind: Block| -> String {
            let same = |p: &&AssembledBlock| p.kind == kind;
            whole
                .blocks()
                .filter(same)
                .map(|p| p.text.as_str())
                .collect()
        };
        let (text, reasoning) = (join(Block::Text), join(Block::Reasoning));
        let fields: Vec<_> = whole
            .items()
            .iter()
            .filter_map(|item| match item {
                AssembledItem::Native(NativeValue {
                    format: f,
                    value: raw,
                }) if **f == *OPENAI => call_fields_of(raw),
                _ => None,
            })
            .collect();
        let calls: Vec<Value> = whole
            .blocks()
            .filter_map(|p| match &p.kind {
                Block::ToolCall { id, name } => {
                    let arguments = if p.text.is_empty() { "{}" } else { &p.text };
                    let mut call = call_json(None, Some(id), Some(name), arguments);
                    for (_, entry, function) in fields.iter().filter(|(b, ..)| *b == p.block) {
                        merge_call(&mut call, entry, function);
                    }
                    Some(call)
                }
                _ => None,
            })
            .collect();
        let mut message = json!({ "role": "assistant", "content": text });
        if !reasoning.is_empty() {
            message["reasoning_content"] = reasoning.into();
        }
        if !calls.is_empty() {
            if text.is_empty() {
                message["content"] = Value::Null;
            }
            message["tool_calls"] = calls.into();
        }
        let finish = whole.finish.as_ref().unwrap_or(&Finish::Stop);
        let mut usage_more = Map::new();
        let mut response = Map::new();
        let mut choices = vec![json!({
            "index": 0,
            "message": message,
            "finish_reason": FINISH.reason(finish)?,
        })];
        for item in whole.items() {
            let AssembledItem::Native(NativeValue { format, value: raw }) = item else {
                continue;
            };
            if &**format == OPENAI && call_fields_of(raw).is_some() {
                continue;
            }
            if let (true, Some(fields)) = (&**format == OPENAI, usage_fields(raw)) {
                usage_more.extend(fields.clone());
                continue;
            }
            if let (true, Some(fields)) = (&**format == OPENAI, response_of(raw)) {
                merge_into(&mut response, fields);
                continue;
            }
            // Another format's goes: only a client of that format could read it.
            if let (true, Some(more)) = (&**format == OPENAI, raw["choices"].as_array()) {
                more.iter().for_each(|c| merge_choice(&mut choices, c));
            }
        }
        let mut body = envelope(whole.start.as_ref(), "chat.completion", unix_now());
        merge_into(&mut body, &response);
        body.insert("choices".into(), choices.into());
        body.insert("usage".into(), usage_body(&whole.usage, &usage_more));
        Ok(body.into())
    }

    fn stream_encoder(&self, asked: Asked) -> Box<dyn Encoder<Output>> {
        Box::new(Chunks::new(asked.usage))
    }

    fn error_body(fault: &Fault) -> Value {
        let code = fault.code();
        let error =
            json!({ "message": fault.message(), "type": code, "param": null, "code": code });
        json!({ "error": merge_details(error, fault) })
    }

    /// Asks a provider stream for usage, which the encoder hides from a client that
    /// did not ask.
    fn prepare_call(v: &mut Map<String, Value>) {
        let usage = v.get("stream_options").and_then(|o| o.get("include_usage"));
        if v.get("stream") != Some(&Value::Bool(true)) || usage == Some(&Value::Bool(true)) {
            return;
        }
        let options = v.entry("stream_options").or_insert(Value::Null);
        match options.as_object_mut() {
            Some(o) => drop(o.insert("include_usage".into(), true.into())),
            None => *options = json!({ "include_usage": true }),
        }
    }

    /// The client's `max_tokens`, if it spelled its limit so, for the
    /// `max_completion_tokens` written: a compatible server may read only
    /// the spelling its clients send.
    fn respell(original: &Value, v: &mut Map<String, Value>) {
        if original.get("max_tokens").is_some()
            && let Some(limit) = v.get("max_completion_tokens").cloned()
        {
            v.insert("max_tokens".into(), limit);
            if original.get("max_completion_tokens").is_none() {
                v.remove("max_completion_tokens");
            }
        }
    }
}

/// A response's usage, as the kind counts it, from its usage object `u`.
fn usage_of(u: &Value) -> Result<Usage, Fault> {
    let n = |ptr| Ok::<_, Fault>(count(u.pointer(ptr))?.unwrap_or(0));
    let mut usage = Usage::new(n("/prompt_tokens")?, n("/completion_tokens")?);
    usage.cached_input_tokens = n("/prompt_tokens_details/cached_tokens")?;
    usage.cache_write_input_tokens = n("/prompt_tokens_details/cache_write_tokens")?;
    usage.reasoning_tokens = n("/completion_tokens_details/reasoning_tokens")?;
    Ok(usage)
}

/// What a usage object `u` says that has no unit: vendor fields, and
/// details beside the cached and reasoning counts.
fn extra_usage(u: &Value) -> Map<String, Value> {
    let known = ["prompt_tokens", "completion_tokens", "total_tokens"];
    let details: [(&str, &[&str]); 2] = [
        (
            "prompt_tokens_details",
            &["cached_tokens", "cache_write_tokens"],
        ),
        ("completion_tokens_details", &["reasoning_tokens"]),
    ];
    usage_extra(u, &known, &details)
}

/// Usage as this format reports it.
fn usage_body(u: &Usage, native: &Map<String, Value>) -> Value {
    let (input, output) = (u.input_tokens, u.output_tokens);
    let mut body = json!({
        "prompt_tokens": input,
        "completion_tokens": output,
        "total_tokens": input.saturating_add(output),
    });
    if u.cached_input_tokens > 0 {
        body["prompt_tokens_details"]["cached_tokens"] = u.cached_input_tokens.into();
    }
    if u.cache_write_input_tokens > 0 {
        body["prompt_tokens_details"]["cache_write_tokens"] = u.cache_write_input_tokens.into();
    }
    if u.reasoning_tokens > 0 {
        body["completion_tokens_details"] = json!({ "reasoning_tokens": u.reasoning_tokens });
    }
    merge_fields(&mut body, native);
    body
}

/// The usage fields an [`Output::Native`] says, if it says only those.
fn usage_fields(raw: &Value) -> Option<&Map<String, Value>> {
    let only = raw.as_object().filter(|o| o.len() == 1)?;
    only.get("usage")?.as_object()
}

/// The response's fields an [`Output::Native`] says, if it says only those.
fn response_of(raw: &Value) -> Option<&Map<String, Value>> {
    let only = raw.as_object().filter(|o| o.len() == 1)?;
    only.get("response")?.as_object()
}
