//! Google Gemini: `POST /v1beta/models/{model}:generateContent` and
//! `:streamGenerateContent?alt=sse`, JSON or SSE, of the conversation kind,
//! as a [`Schema`] ([`GeminiGenerate`]).
//!
//! The model is in the path, not the body: a client's request names it there
//! (`models/` is left off), and a call to a provider writes it there from the
//! input's model. A stream is asked for by the method (`streamGenerateContent`),
//! not by a field of the body: with `alt=sse` it is SSE, without it Gemini's
//! other framing, one JSON array whose elements are the same responses. The
//! provider is always asked for SSE.
//!
//! # Requests
//!
//! `contents` are messages (`user`; `model` is the assistant), their parts
//! the kind's: `text`, a `thought` part as reasoning, `inlineData` and
//! `fileData` as an image, audio or document by their MIME type,
//! `functionCall` as a tool call and `functionResponse` as its result. Gemini
//! names a call by tool and position unless it sends an `id`: a call without
//! one is given an id of this format's making (`gemini_call_..`), which its
//! answer finds by name, and which is never sent back to Gemini. A
//! `functionResponse`'s `response` object is the result's text, as JSON;
//! writing it, an object stays as it is, other text goes under `output`
//! (`error` for a failed call). `systemInstruction` is a leading system
//! message; `tools` are the function declarations, their `parameters` (or
//! `parametersJsonSchema`) JSON Schema; `toolConfig` gives the tool choice;
//! `generationConfig` the output limit, `temperature`, `topP`, `topK`, stop
//! sequences, structured output (`responseMimeType` with a schema) and
//! `thinkingConfig`: `thinkingLevel` is an effort, `thinkingBudget` a token
//! budget (`0` is off), `includeThoughts` the summary. A `thinkingBudget`
//! beside a `thinkingLevel`, or a dynamic one (`-1`), stays native.
//!
//! What the kind has no form for is this format's `native`, where it
//! appeared, and refused in another format: `safetySettings`,
//! `cachedContent`, other `thinkingConfig` fields, other `generationConfig`
//! fields, tools Google runs, a `toolConfig` the kind has no choice for,
//! video parts, and a part's `thoughtSignature` (which another format drops).
//! A request for several candidates, one tool call at a time, or an end
//! user's id is refused when written.
//!
//! A Gemini 3 model refuses a function call in the history that has no
//! `thoughtSignature`, as a call made in another format has none: written
//! for one, the first call of each `model` content without a signature of
//! its own takes Google's placeholder ([`SIGNATURE_PLACEHOLDER`]), as Gemini
//! signs only the first of a turn's parallel calls. A model before Gemini 3
//! takes none.
//!
//! # Responses as events
//!
//! A stream and a whole body decode into the same events, by these rules:
//!
//! - [`Output::Start`] comes from the first response's `responseId` and
//!   `modelVersion`; its `native` holds its other fields (`promptFeedback`).
//! - The first candidate's parts are blocks, numbered as they open:
//!   consecutive `text` parts are one [`Block::Text`] (a `thought` part a
//!   [`Block::Reasoning`]), a `functionCall` is a [`Block::ToolCall`] whose
//!   arguments are one delta. A part with fields beyond its content, such as
//!   a `thoughtSignature`, is a block of its own, and the fields come before
//!   it as its [`Output::block_fields`].
//! - `finishReason` is the [`Output::Finish`]: `STOP` is a stop, or a
//!   tool use when the response made calls, `MAX_TOKENS` the token limit, and
//!   `SAFETY`, `RECITATION`, `BLOCKLIST` and the like are filtered; any other
//!   is native to this format. A prompt Gemini blocks (`promptFeedback`
//!   with no candidate) is filtered.
//! - `usageMetadata` gives [`Output::Usage`]: `promptTokenCount` is the
//!   input, `candidatesTokenCount` and `thoughtsTokenCount` the output,
//!   `cachedContentTokenCount` the cached input.
//! - What else a response says is an inert [`Output::Native`] of this format
//!   (`candidate_fields`, `response_fields`, `usage_fields`), which another
//!   format drops, and what the kind has no form for is not inert:
//!   a part of another kind (`executableCode`, a response image), and a
//!   second candidate.
//!
//! A stream ends with its last response, which has the `finishReason`; one
//! that ends without it was cut. Encoders do the reverse: a stream is a
//! response per delta, a tool call whole once it ends, and the last with
//! the finish and usage; a body is written from the events collected.

mod decode;
mod encode;
mod request;
#[cfg(test)]
mod tests;

use crate::is_inert;
use crate::schema::{
    Asked, Keep, Names, NativeFieldsExt, Schema, UsageFields, block_fields, check_usage, count,
    kept_fields, merge_details, merge_fields, native_fields, set, unwritable_content,
    unwritable_native, usage_extra,
};
use respan_gateway_conversation::{Assembled, AssembledItem, Block, Finish, Input, Output, Usage};
use respan_gateway_core::Provider;
use respan_gateway_core::{Connection, Fault, Format, NativeValue};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::{self, StatusCode, request::Parts as RequestHead};
use respan_gateway_http::{ClientIo, Encoder, ProviderIo};
use respan_gateway_http::{Connect, KeyIn, Upstream};
use serde_json::{Map, Value, json};

use decode::GeminiDecoder;
use encode::{EventStream, JsonArray, ResponseFields, part_of};
pub use request::needs_signatures;
use request::{
    read_config, read_messages, read_tools, sign_calls, split_system, write_config, write_messages,
    write_tools,
};

/// This format's name.
const GEMINI: &str = "gemini-generate";

/// What a Gemini 3 model accepts as the signature of a function call made
/// elsewhere, as Google documents it.
pub const SIGNATURE_PLACEHOLDER: &str = "skip_thought_signature_validator";

/// What starts the id of a tool call Gemini named none for.
const SYNTHETIC: &str = "gemini_call_";

const ROLES: Names<respan_gateway_conversation::Role> = Names(
    &[
        ("user", respan_gateway_conversation::Role::User),
        ("model", respan_gateway_conversation::Role::Assistant),
    ],
    GEMINI,
);

/// The reasons the kind has; `STOP` is also a tool use, which a response
/// that made calls says so.
const FINISH: Names<Finish> = Names(
    &[
        ("STOP", Finish::Stop),
        ("MAX_TOKENS", Finish::Length),
        ("STOP", Finish::ToolUse),
        ("SAFETY", Finish::Filtered),
    ],
    GEMINI,
);

/// The finish reason of a prompt Gemini blocked, which has no candidate.
const BLOCKED: &str = "PROMPT_BLOCKED";

/// A reason as a [`Finish`]: those that filter the answer, as the kind has
/// it, stay Gemini's own for a client of this format.
fn finish_of(reason: &str) -> Finish {
    match reason {
        "RECITATION"
        | "BLOCKLIST"
        | "PROHIBITED_CONTENT"
        | "SPII"
        | "IMAGE_SAFETY"
        | "IMAGE_PROHIBITED_CONTENT"
        | "IMAGE_RECITATION"
        | "LANGUAGE" => Finish::native_or(GEMINI, reason, Finish::Filtered),
        _ => FINISH.finish(reason),
    }
}

/// The model, unescaped, and the method a Gemini path names:
/// `.../models/{model}:{method}`, `generateContent` or
/// `streamGenerateContent`.
pub fn call_of(uri: &http::Uri) -> Option<(String, &str)> {
    let path = uri.path();
    let (rest, last) = path.rsplit_once('/')?;
    let (model, method) = last.rsplit_once(':')?;
    let named = rest.rsplit('/').next() == Some("models") && !model.is_empty();
    let known = matches!(method, "generateContent" | "streamGenerateContent");
    let unescaped = || percent_encoding::percent_decode_str(model).decode_utf8_lossy();
    (named && known).then(|| (unescaped().into_owned(), method))
}

/// Whether a streaming call at `uri` asks for server-sent events (`alt=sse`).
pub fn sse(uri: &http::Uri) -> bool {
    uri.query()
        .is_some_and(|q| q.split('&').any(|p| p == "alt=sse"))
}

/// Google Gemini's `generateContent`, named `gemini-generate`: a `POST` to a
/// path `.../models/{model}:generateContent` or `:streamGenerateContent`.
/// The provider's base URL includes the version, such as
/// `https://generativelanguage.googleapis.com/v1beta`, and its transport the
/// key (`x-goog-api-key`).
#[derive(Debug, Clone, Copy, Default)]
pub struct GeminiGenerate;

impl GeminiGenerate {
    /// This format's name, `gemini-generate`.
    pub fn name(&self) -> &'static str {
        GEMINI
    }
}

impl Format<ClientIo, Input, Output> for GeminiGenerate {
    fn name(&self) -> &str {
        GEMINI
    }
    fn matches(&self, io: &ClientIo) -> bool {
        self.accepts(io.request())
    }
    fn bind(&self, io: ClientIo) -> Result<Connection<Input, Output>, Fault> {
        self.bind_client(io)
    }
}

/// Its spec's: the key in `x-goog-api-key`, over HTTP.
impl Upstream for GeminiGenerate {
    fn name(&self) -> &str {
        GEMINI
    }

    fn key(&self) -> KeyIn {
        KeyIn::Header("x-goog-api-key")
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for GeminiGenerate {
    fn name(&self) -> &str {
        GEMINI
    }
    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        self.bind_provider(io)
    }
}

/// Keys the kind has a place for, or that change no output.
const KNOWN: &[&str] = &[
    "contents",
    "systemInstruction",
    "tools",
    "toolConfig",
    "generationConfig",
    "model",
];

/// Whether a model name is one a path can hold as it is.
fn is_segment(model: &str) -> bool {
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
    !model.is_empty() && model != "." && model != ".." && model.chars().all(plain)
}

impl GeminiGenerate {
    /// A call's request: its body, and the model its path names.
    fn decode_call(&self, head: &RequestHead, raw: Bytes) -> Result<Input, Fault> {
        let v = match serde_json::from_slice::<Value>(&raw) {
            Ok(v) if v.is_object() => v,
            _ => return Err(Fault::bad_request("request body is not a JSON object")),
        };
        let mut chat = self.decode_json(&v);
        chat.model = call_of(&head.uri).map(|(model, _)| model);
        Ok(chat)
    }
}

/// Gemini counts no cache writes.
const USAGE: UsageFields = UsageFields {
    cache_writes: false,
    reasoning: true,
};

impl Schema for GeminiGenerate {
    const NAME: &'static str = GEMINI;
    const PATH: &'static str = "/models/{model}:generateContent";
    const STREAM_FLAG: bool = false;
    const OWN: &'static [&'static str] = &["generationConfig", "tools", "toolConfig"];
    type Decoding = GeminiDecoder;

    fn decoder(&self) -> GeminiDecoder {
        GeminiDecoder::default()
    }

    fn decode_body(&self, v: &Value, out: &mut Vec<Output>) -> Result<(), Fault> {
        self.decoder().body(v, out)
    }

    fn accepts<B>(&self, request: &http::Request<B>) -> bool {
        request.method() == http::Method::POST && call_of(request.uri()).is_some()
    }

    fn read_request(
        &self,
        head: &http::request::Parts,
    ) -> Box<dyn respan_gateway_http::Decoder<Input>> {
        let (schema, head) = (*self, head.clone());
        Box::new(move |raw: Bytes, out: &mut Vec<Input>| {
            out.push(schema.decode_call(&head, raw)?);
            Ok(())
        })
    }

    /// The model is in the path, not in the body.
    fn encode_request(&self, chat: &Input) -> Result<Map<String, Value>, Fault> {
        let keep = Keep {
            own: Self::OWN,
            droppable: &[],
        };
        let in_the_request = |key: &str| format!("'{key}' in the request");
        let native = kept_fields(GEMINI, &chat.native, in_the_request, keep)?;
        let mut v = self.encode_json(chat)?;
        v.extend(native);
        if needs_signatures(chat.model.as_deref()) {
            sign_calls(&mut v);
        }
        Ok(v)
    }

    fn target(&self, input: &Input, stream: bool) -> Result<Option<String>, Fault> {
        let model = input.model.as_deref().unwrap_or_default();
        let model = model.strip_prefix("models/").unwrap_or(model);
        if !is_segment(model) {
            return Err(Fault::bad_request(
                "a Gemini call needs a model, such as gemini-2.5-flash",
            ));
        }
        Ok(Some(match stream {
            true => format!("/models/{model}:streamGenerateContent?alt=sse"),
            false => format!("/models/{model}:generateContent"),
        }))
    }

    /// The streaming method is named in the path.
    fn streams(uri: &http::Uri) -> bool {
        call_of(uri).is_some_and(|(_, method)| method == "streamGenerateContent")
    }

    /// A stream without `alt=sse` is Gemini's other framing: a JSON array,
    /// its elements the responses the SSE events carry.
    fn response_encoder(
        &self,
        head: &mut http::response::Parts,
        request: &http::Request<Bytes>,
    ) -> Box<dyn Encoder<Output>> {
        let encoder = crate::schema::response_encoder(*self, head, request);
        let array = Self::streams(request.uri()) && !sse(request.uri()) && head.status.is_success();
        if !array {
            return encoder;
        }
        head.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        Box::new(JsonArray::new(encoder))
    }

    fn decode_part(_: &Value) -> Option<respan_gateway_conversation::Content> {
        None
    }

    fn encode_part(_: &respan_gateway_conversation::Content) -> Result<Value, Fault> {
        Err(unwritable_content())
    }

    fn stream_encoder(&self, _asked: Asked) -> Box<dyn Encoder<Output>> {
        Box::new(EventStream::default())
    }

    /// What is kept beside a response: its fields, a candidate's, usage's.
    fn is_inert(raw: &Value) -> bool {
        matches!(
            raw["type"].as_str(),
            Some("candidate_fields" | "response_fields" | "usage_fields")
        )
    }

    fn decode_json(&self, v: &Value) -> Input {
        let mut chat = Input::default();
        let rest = crate::schema::unsupported_fields(v, KNOWN, &[]);
        chat.native = native_fields(GEMINI, rest);
        read_messages(v, &mut chat);
        read_tools(v, &mut chat);
        read_config(v, &mut chat);
        chat.native = native_fields(GEMINI, std::mem::take(chat.native.fields_mut(GEMINI)));
        chat
    }

    fn encode_json(&self, chat: &Input) -> Result<Map<String, Value>, Fault> {
        let (system, rest) = split_system(chat)?;
        if rest.is_empty() {
            return Err(Fault::bad_request(
                "Gemini needs at least one user or model message",
            ));
        }
        let mut v = Map::new();
        set(&mut v, "systemInstruction", system);
        v.insert("contents".into(), write_messages(rest)?.into());
        write_tools(chat, &mut v)?;
        write_config(chat, &mut v)?;
        Ok(v)
    }

    fn is_response(v: &Value) -> bool {
        ["candidates", "promptFeedback", "usageMetadata"]
            .iter()
            .any(|k| v.get(*k).is_some())
    }

    fn encode_whole(&self, whole: &Assembled) -> Result<Value, Fault> {
        check_usage(&whole.usage, USAGE)?;
        let mut body = ResponseFields::default();
        let mut parts: Vec<Value> = Vec::new();
        let mut fields: Vec<(u32, Map<String, Value>)> = Vec::new();
        for item in whole.items() {
            match item {
                AssembledItem::Block(part) => {
                    let mut written = match &part.kind {
                        Block::Text | Block::Reasoning if part.text.is_empty() => {
                            if !fields.iter().any(|(b, _)| *b == part.block) {
                                continue;
                            }
                            part_of(&part.kind, "")?
                        }
                        kind => part_of(kind, &part.text)?,
                    };
                    if let Some(i) = fields.iter().position(|(b, _)| *b == part.block) {
                        merge_fields(&mut written, &fields.remove(i).1);
                    }
                    parts.push(written);
                }
                AssembledItem::Native(NativeValue { format, value: raw })
                    if **format == *GEMINI =>
                {
                    match (block_fields(raw), raw["type"].as_str()) {
                        (Some((block, f)), _) => fields.push((block, f.clone())),
                        (None, Some("part")) => parts.push(raw["part"].clone()),
                        _ if body.add(raw) => {}
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
        let finish = whole.finish.as_ref().unwrap_or(&Finish::Stop);
        body.finish(whole.start.as_ref(), parts, finish, &whole.usage)
    }

    fn error_body(fault: &Fault) -> Value {
        let status = respan_gateway_http_faults::status(fault);
        let error = json!({
            "code": status.as_u16(),
            "message": fault.message(),
            "status": google_status(status),
        });
        let mut error = merge_details(error, fault);
        // A provider of another format sends a code of its own, as a string.
        error["code"] = status.as_u16().into();
        if !error["status"].as_str().is_some_and(is_status_name) {
            error["status"] = google_status(status).into();
        }
        json!({ "error": error })
    }
}

/// Whether `name` is a `google.rpc.Code` name, such as `NOT_FOUND`.
pub fn is_status_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_uppercase() || c == '_')
}

/// The HTTP status Google answers a `google.rpc.Code` name with; 500 for
/// one it does not know.
pub fn status_code(name: &str) -> u16 {
    match name {
        "INVALID_ARGUMENT" | "FAILED_PRECONDITION" | "OUT_OF_RANGE" => 400,
        "UNAUTHENTICATED" => 401,
        "PERMISSION_DENIED" => 403,
        "NOT_FOUND" => 404,
        "ABORTED" | "ALREADY_EXISTS" => 409,
        "RESOURCE_EXHAUSTED" => 429,
        "CANCELLED" => 499,
        "UNIMPLEMENTED" => 501,
        "UNAVAILABLE" => 503,
        "DEADLINE_EXCEEDED" => 504,
        _ => 500,
    }
}

/// The `google.rpc.Code` name Google gives an HTTP `status`.
fn google_status(status: StatusCode) -> &'static str {
    match status.as_u16() {
        400 => "INVALID_ARGUMENT",
        401 => "UNAUTHENTICATED",
        403 => "PERMISSION_DENIED",
        404 => "NOT_FOUND",
        409 => "ABORTED",
        429 => "RESOURCE_EXHAUSTED",
        499 => "CANCELLED",
        501 => "UNIMPLEMENTED",
        502 | 503 => "UNAVAILABLE",
        504 => "DEADLINE_EXCEEDED",
        s if s >= 500 => "INTERNAL",
        _ => "FAILED_PRECONDITION",
    }
}

/// Usage as `usageMetadata` has it, if there is any: the output without the
/// thoughts as `candidatesTokenCount`, and their sum as the total.
pub fn usage_body(u: &Usage, native: &Map<String, Value>) -> Option<Value> {
    if u.is_empty() && native.is_empty() {
        return None;
    }
    let tool_use = native
        .get("toolUsePromptTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let mut body = json!({
        "promptTokenCount": u.input_tokens,
        "totalTokenCount": u.input_tokens.saturating_add(u.output_tokens).saturating_add(tool_use),
    });
    let candidates = u.output_tokens.saturating_sub(u.reasoning_tokens);
    if candidates > 0 {
        body["candidatesTokenCount"] = candidates.into();
    }
    if u.cached_input_tokens > 0 {
        body["cachedContentTokenCount"] = u.cached_input_tokens.into();
    }
    if u.reasoning_tokens > 0 {
        body["thoughtsTokenCount"] = u.reasoning_tokens.into();
    }
    merge_fields(&mut body, native);
    Some(body)
}

/// Usage totals of a `usageMetadata`; the thoughts are part of the output.
pub fn usage_of(u: &Value) -> Result<Usage, Fault> {
    let n = |key: &str| count(u.get(key));
    let thoughts = n("thoughtsTokenCount")?.unwrap_or(0);
    Ok(Usage {
        input_tokens: n("promptTokenCount")?.unwrap_or(0),
        output_tokens: n("candidatesTokenCount")?
            .unwrap_or(0)
            .saturating_add(thoughts),
        cached_input_tokens: n("cachedContentTokenCount")?.unwrap_or(0),
        reasoning_tokens: thoughts,
        ..Usage::default()
    })
}

/// What a usage object says that has no unit.
fn extra_usage(u: &Value) -> Map<String, Value> {
    let known = [
        "promptTokenCount",
        "candidatesTokenCount",
        "thoughtsTokenCount",
        "cachedContentTokenCount",
        "totalTokenCount",
    ];
    usage_extra(u, &known, &[])
}
