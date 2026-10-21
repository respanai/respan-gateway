//! OpenAI Responses: `POST /responses`, JSON or SSE, of the conversation
//! kind, as a [`Schema`] ([`OpenAiResponses`]).
//!
//! # Requests
//!
//! `instructions` is a leading system message and `input` the messages after
//! it: a string is one user message, and an array's items are messages
//! (`input_text`, `input_image` and `input_file` parts), an assistant's
//! function calls and reasoning, and function call outputs as the results of
//! a user message. Consecutive items of an assistant are one assistant
//! message (`request` says how an item's own fields are kept). Function
//! tools, `tool_choice`, `parallel_tool_calls`, `max_output_tokens`,
//! `temperature`, `top_p`, `user`, `text.format` and `reasoning` (its `effort`,
//! and a `summary` of `auto`) are the kind's.
//!
//! A call whose client speaks another format, which can name no response,
//! asks to store nothing (`store: false`) and for its reasoning encrypted
//! (`include: ["reasoning.encrypted_content"]`), as that reasoning, kept
//! native to its block, is the only way it reaches a later call (the
//! gateway's `plugins.responses` puts it back).
//!
//! What the kind has no form for stays native to this format and is refused
//! in translation: `previous_response_id` and `conversation` (another
//! provider cannot resolve them; the gateway's `plugins.responses` resolves
//! the first itself), `include`, `truncation`,
//! the rest of `reasoning` (`generate_summary`, a summary but `auto`), `metadata`, built-in tools (web search,
//! file search, computer use, code interpreter, MCP) and the items they
//! make, and the rest of what the request says beyond the spec's defaults.
//! Item ids and statuses, and the default `strict` of a function tool, are
//! inert: any other format drops them. A request with stop sequences cannot be
//! written, as Responses has none.
//!
//! # Responses as events
//!
//! A stream and a whole body decode into the same events, by these rules:
//!
//! - [`Output::Start`] comes from `response.created` (or the body), with the
//!   response's own fields, such as the settings it echoes, in its `native`;
//!   those a later event changes are one [`Output::Native`]
//!   `{"type": "response_fields", "fields": {..}}`.
//! - A message's output text is a [`Block::Text`](respan_gateway_conversation::Block::Text) (one per content part), a
//!   `function_call` a [`Block::ToolCall`](respan_gateway_conversation::Block::ToolCall) named by its `call_id`, and a
//!   `reasoning` item a [`Block::Reasoning`](respan_gateway_conversation::Block::Reasoning) of its summary, whose parts are
//!   joined as paragraphs. A message's text streamed without its part's
//!   `response.content_part.added`, as some vendors send it, opens the part
//!   at its first delta. Blocks are numbered in the order they open;
//!   `output_index`, `content_index`, `summary_index` and `sequence_number`
//!   are not the kind's, and are written anew.
//! - The terminal event gives the usage ([`Output::Usage`]), and the status
//!   the [`Output::Finish`]: `completed` is a stop, or a tool use if the
//!   response called a function, and `incomplete` its reason
//!   (`max_output_tokens` the token limit, `content_filter` a filter, else a
//!   native reason). `response.failed` and `error` are the stream's failure.
//! - The item's fields the kind has no place for, such as its `id`, are its
//!   [`Output::block_fields`] before its block opens, and an annotation one
//!   native `{"type": "annotation", ..}`; the usage object's other fields
//!   are `{"type": "usage_fields", ..}`. Another format drops the block's
//!   fields and the usage's, and refuses annotations.
//! - An item the kind has no form for (a built-in tool's call, a refusal)
//!   is a native of each of its events as they came, or, from a whole body,
//!   one `response.output_item.done`; any other format refuses it.
//!
//! Encoders do the reverse. A stream is written as the documented sequence
//! (`response.created`, `response.in_progress`, each item's `added`, content
//! part, delta, `done` events, and the terminal event with the response
//! whole), one item at a time, with ids and indexes made when the events
//! have none. A whole body is the response, its `output` the blocks in the
//! order they opened.
//!
//! Not carried: log probabilities, which are written empty.

mod decode;
mod encode;
mod request;
#[cfg(test)]
mod tests;

use crate::schema::{
    Asked, INERT, NativeFieldsExt, Schema, UsageFields, count, merge_details, merge_fields,
    native_fields, unsupported_fields, unwritable_content, usage_extra,
};

use respan_gateway_conversation::{Assembled, Content, Finish, Input, Output, Start, Usage};
use respan_gateway_core::Provider;
use respan_gateway_core::{Connection, Fault, Format};
use respan_gateway_http::{ClientIo, Encoder, ProviderIo};
use respan_gateway_http::{Connect, Upstream};
use serde_json::{Map, Value, json};

pub use decode::ResponsesDecoder;

/// The wire format's name ([`Schema::NAME`]).
const RESPONSES: &str = "openai-responses";

/// The type of the native that carries an annotation of a text block.
const ANNOTATION: &str = "annotation";
/// The type of the native that carries the usage's other fields.
const USAGE_FIELDS: &str = "usage_fields";
/// The type of the native that carries the response's fields that changed.
const RESPONSE_FIELDS: &str = "response_fields";

/// OpenAI Responses, `openai-responses`: a `POST` to any path ending in
/// `/responses`. The provider's base URL includes the version, such as
/// `https://api.openai.com/v1`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenAiResponses;

impl OpenAiResponses {
    /// Whether a request `body` has `instructions`, which this format reads
    /// as its leading system message, as it reads them; reading no more of
    /// the body than that field.
    pub fn instructed(body: &[u8]) -> bool {
        #[derive(serde::Deserialize)]
        struct Instructions {
            #[serde(default)]
            instructions: Value,
        }
        serde_json::from_slice::<Instructions>(body)
            .is_ok_and(|b| crate::schema::says(&b.instructions))
    }
}

impl Format<ClientIo, Input, Output> for OpenAiResponses {
    fn name(&self) -> &str {
        RESPONSES
    }
    fn matches(&self, io: &ClientIo) -> bool {
        self.accepts(io.request())
    }
    fn bind(&self, io: ClientIo) -> Result<Connection<Input, Output>, Fault> {
        self.bind_client(io)
    }
}

/// Its spec's: a bearer key, over HTTP, beside its base URL.
impl Upstream for OpenAiResponses {
    fn name(&self) -> &str {
        RESPONSES
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for OpenAiResponses {
    fn name(&self) -> &str {
        RESPONSES
    }
    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        self.bind_provider(io)
    }
}

/// Keys a conversation has a place for, or that change no output.
const KNOWN: &[&str] = &[
    "model",
    "input",
    "instructions",
    "stream",
    "stream_options",
    "max_output_tokens",
    "temperature",
    "top_p",
];

/// Keys that change nothing at these values, as SDKs send by default.
const DEFAULTS: &[(&str, f64)] = &[("store", 1.0), ("background", 0.0)];

const USAGE: UsageFields = UsageFields {
    cache_writes: true,
    reasoning: true,
};

impl Schema for OpenAiResponses {
    const NAME: &'static str = RESPONSES;
    const PATH: &'static str = "/responses";
    const CARRIED: &'static [&'static str] =
        &["openai-beta", "openai-organization", "openai-project"];
    /// The structured output requirement is in `text`.
    const OWN: &'static [&'static str] = &["text", "reasoning"];
    /// Chat Completions' fields Responses has too, OpenAI's both.
    const ADOPTED: &'static [(&'static str, &'static str, &'static str)] = &[
        (crate::openai::OPENAI, "metadata", "metadata"),
        (crate::openai::OPENAI, "service_tier", "service_tier"),
        (
            crate::openai::OPENAI,
            "prompt_cache_key",
            "prompt_cache_key",
        ),
        (
            crate::openai::OPENAI,
            "safety_identifier",
            "safety_identifier",
        ),
        (crate::openai::OPENAI, "verbosity", "text.verbosity"),
    ];
    type Decoding = ResponsesDecoder;

    /// A client of another format can name no response: none is stored,
    /// and its reasoning comes encrypted, to be carried to a later call.
    fn translated(v: &mut Map<String, Value>) {
        v.entry("store").or_insert(false.into());
        let include = v.entry("include").or_insert_with(|| json!([]));
        if let Some(include) = include.as_array_mut() {
            let encrypted = json!("reasoning.encrypted_content");
            if !include.contains(&encrypted) {
                include.push(encrypted);
            }
        }
    }

    fn decoder(&self) -> ResponsesDecoder {
        ResponsesDecoder::default()
    }

    fn decode_body(&self, v: &Value, out: &mut Vec<Output>) -> Result<(), Fault> {
        self.decoder().body(v, out)
    }

    fn decode_json(&self, v: &Value) -> Input {
        let mut chat = Input::default();
        chat.options.max_output_tokens = v["max_output_tokens"].as_u64();
        chat.options.temperature = v["temperature"].as_f64();
        chat.options.top_p = v["top_p"].as_f64();
        chat.user = v["user"].as_str().map(Into::into);
        chat.native = native_fields(RESPONSES, unsupported_fields(v, KNOWN, DEFAULTS));
        if chat.user.is_some() {
            chat.native.fields_mut(RESPONSES).shift_remove("user");
        }
        // Not to store a response is OpenAI's to honour; another provider
        // stores nothing anyway.
        if let Some(store) = chat.native.fields_mut(RESPONSES).shift_remove("store") {
            let inert = serde_json::json!({ "store": store });
            chat.native
                .fields_mut(RESPONSES)
                .insert(INERT.into(), inert);
        }
        request::read_input(v, &mut chat);
        request::read_tools(v, &mut chat);
        chat.native = native_fields(RESPONSES, std::mem::take(chat.native.fields_mut(RESPONSES)));
        request::read_format(v, &mut chat);
        request::read_reasoning(&mut chat);
        chat
    }

    fn encode_json(&self, chat: &Input) -> Result<Map<String, Value>, Fault> {
        if !chat.options.stop.is_empty() {
            return Err(crate::schema::untranslatable("stop sequences in Responses"));
        }
        let mut v = Map::new();
        request::write_input(chat, &mut v)?;
        request::write_tools(chat, &mut v)?;
        crate::schema::set(
            &mut v,
            "max_output_tokens",
            chat.options.max_output_tokens.map(Into::into),
        );
        crate::schema::set(
            &mut v,
            "temperature",
            chat.options.temperature.map(Into::into),
        );
        crate::schema::set(&mut v, "top_p", chat.options.top_p.map(Into::into));
        crate::schema::set(&mut v, "user", chat.user.clone().map(Into::into));
        request::write_format(chat, &mut v);
        request::write_reasoning(chat, &mut v)?;
        if chat.options.top_k.is_some() {
            return Err(crate::schema::untranslatable("top_k in Responses"));
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
        v["object"] == "response"
    }

    fn encode_whole(&self, whole: &Assembled) -> Result<Value, Fault> {
        encode::whole(whole)
    }

    fn stream_encoder(&self, _asked: Asked) -> Box<dyn Encoder<Output>> {
        Box::new(encode::Events::default())
    }

    fn error_body(fault: &Fault) -> Value {
        let code = fault.code();
        let error =
            json!({ "message": fault.message(), "type": code, "param": null, "code": code });
        json!({ "error": merge_details(error, fault) })
    }

    /// What is kept beside a response: the fields of an item or of the
    /// usage, those the response changed, and a reasoning's raw text.
    fn is_inert(raw: &Value) -> bool {
        let kind = raw["type"].as_str().unwrap_or_default();
        matches!(kind, USAGE_FIELDS | RESPONSE_FIELDS)
            || kind.starts_with("response.reasoning_text.")
    }
}

/// A response's usage, as the kind counts it, from its usage object `u`.
pub(crate) fn usage_of(u: &Value) -> Result<Usage, Fault> {
    let n = |ptr| Ok::<_, Fault>(count(u.pointer(ptr))?.unwrap_or(0));
    let mut usage = Usage::new(n("/input_tokens")?, n("/output_tokens")?);
    usage.cached_input_tokens = n("/input_tokens_details/cached_tokens")?;
    usage.cache_write_input_tokens = n("/input_tokens_details/cache_write_tokens")?;
    usage.reasoning_tokens = n("/output_tokens_details/reasoning_tokens")?;
    Ok(usage)
}

/// What a usage object `u` says that has no unit.
pub(crate) fn extra_usage(u: &Value) -> Map<String, Value> {
    let known = ["input_tokens", "output_tokens", "total_tokens"];
    let details: [(&str, &[&str]); 2] = [
        (
            "input_tokens_details",
            &["cached_tokens", "cache_write_tokens"],
        ),
        ("output_tokens_details", &["reasoning_tokens"]),
    ];
    usage_extra(u, &known, &details)
}

/// Usage as this format reports it.
fn usage_body(u: &Usage, more: &Map<String, Value>) -> Value {
    let mut input = json!({ "cached_tokens": u.cached_input_tokens });
    if u.cache_write_input_tokens > 0 {
        input["cache_write_tokens"] = u.cache_write_input_tokens.into();
    }
    let mut body = json!({
        "input_tokens": u.input_tokens,
        "input_tokens_details": input,
        "output_tokens": u.output_tokens,
        "output_tokens_details": { "reasoning_tokens": u.reasoning_tokens },
        "total_tokens": u.input_tokens.saturating_add(u.output_tokens),
    });
    merge_fields(&mut body, more);
    body
}

/// The fields of a response object that are the response's own, not the
/// kind's: all but its id, model, status, output and usage, and what the
/// terminal event sets.
pub(crate) fn response_fields(r: &Value) -> Map<String, Value> {
    let known = [
        "id",
        "model",
        "object",
        "status",
        "completed_at",
        "output",
        "usage",
        "incomplete_details",
        "error",
    ];
    unsupported_fields(r, &known, &[])
}

/// The status a response with this `finish` has, and its reason if it is
/// incomplete.
pub(crate) fn status_of(finish: &Finish) -> Result<(&'static str, Option<String>), Fault> {
    Ok(match finish {
        Finish::Stop | Finish::ToolUse => ("completed", None),
        Finish::Length => ("incomplete", Some("max_output_tokens".into())),
        Finish::Filtered => ("incomplete", Some("content_filter".into())),
        Finish::Native { format, reason, .. } if **format == *RESPONSES => (
            "incomplete",
            Some(reason.clone()).filter(|r| r != "incomplete"),
        ),
        Finish::Native {
            fallback: Some(fallback),
            ..
        } => status_of(fallback)?,
        _ => return Err(unwritable_content()),
    })
}

/// The response object: what a response says of its request when the
/// provider did not, then the provider's own fields, then the kind's.
pub(crate) fn response_body(
    start: Option<&Start>,
    created: u64,
    status: &str,
    reason: Option<&str>,
    output: Vec<Value>,
    usage: Option<(&Usage, &Map<String, Value>)>,
    changed: &Map<String, Value>,
) -> Map<String, Value> {
    let (id, model) = start
        .map(|s| (s.id.as_str(), s.model.as_str()))
        .unwrap_or_default();
    let defaults = [
        ("created_at", json!(created)),
        ("instructions", Value::Null),
        ("max_output_tokens", Value::Null),
        ("metadata", json!({})),
        ("parallel_tool_calls", json!(true)),
        ("previous_response_id", Value::Null),
        ("temperature", Value::Null),
        ("tool_choice", json!("auto")),
        ("tools", json!([])),
        ("top_p", Value::Null),
    ];
    let mut body = Map::new();
    body.extend(defaults.map(|(k, v)| (k.to_owned(), v)));
    let own = start.filter(|s| s.native.format() == Some(RESPONSES));
    let mut fields = Value::Object(own.map(|s| s.native.fields().clone()).unwrap_or_default());
    merge_fields(&mut fields, changed);
    if let Value::Object(fields) = fields {
        body.extend(fields);
    }
    body.insert("id".into(), id.into());
    body.insert("object".into(), "response".into());
    body.insert("model".into(), model.into());
    body.insert("status".into(), status.into());
    body.insert("completed_at".into(), Value::Null);
    body.insert("error".into(), Value::Null);
    let details = reason.map(|r| json!({ "reason": r }));
    body.insert("incomplete_details".into(), details.unwrap_or(Value::Null));
    body.insert("output".into(), output.into());
    let usage = usage.map(|(u, more)| usage_body(u, more));
    body.insert("usage".into(), usage.unwrap_or(Value::Null));
    body
}
