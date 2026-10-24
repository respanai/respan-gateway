//! Amazon Bedrock's Converse: `POST /model/{modelId}/converse` and
//! `/converse-stream`, of the conversation kind, as a [`Schema`]
//! ([`BedrockConverse`]), at both edges: a client of the Bedrock Runtime
//! SDKs (its endpoint the gateway's, its key a bearer token) talks to any
//! model the gateway routes to, and a provider is Bedrock.
//!
//! The model is in the path, escaped whole (an inference profile's id,
//! `us.anthropic.claude-sonnet-4-20250514-v1:0`, or an ARN, is one segment),
//! and the method names a stream: `converse-stream` answers in the AWS event
//! stream ([`respan_gateway_eventstream`]), not in server-sent events.
//!
//! # Requests
//!
//! The leading system messages are the `system` blocks. `messages` are the
//! rest, a message of the role before it joined to that one, as Converse needs
//! the roles to alternate, and a blank text left out; a part, tool or
//! system block with a cache marker (Anthropic's `cache_control`, or a
//! client's `cachePoint`) is followed by a `cachePoint`. A part is a `text`,
//! an `image` or `document` (bytes only: a URL is refused, as the format has
//! no fetch), a `toolUse` or `toolResult` (`status: "error"` for a failed
//! one), or `reasoningContent` that Bedrock signed or redacted, which is the
//! only reasoning sent back. `inferenceConfig` has `maxTokens`,
//! `temperature`, `topP` and `stopSequences`; `toolConfig` the tools and the
//! choice (`auto`, `any`, or one `tool`); `outputConfig` a JSON schema the
//! answer keeps to. A reasoning budget is
//! `additionalModelRequestFields.thinking`, and `Off` its disabling.
//!
//! Refused as having no field: `top_k`, an effort, JSON without a schema, an
//! end user's id, one tool call at a time, audio, a system message after
//! the conversation starts, and the fields of another format.
//!
//! A client's request is read the same way back; what the kind has no form
//! for (a guardrail, a service tier, a guard or video block, media in S3,
//! the model's own `additionalModelRequestFields`) stays native, so only a
//! Bedrock provider is sent it. Its answer is Converse's body, or for
//! `converse-stream` the event stream, its failures an `exception`
//! message; a refusal is `{"message"}` with its type in `x-amzn-errortype`.
//!
//! # Responses as events
//!
//! A stream and a whole body decode into the same events:
//!
//! - [`Output::Start`] comes with the first event (Converse names neither an
//!   id nor a model).
//! - `output.message.content` blocks are numbered as Converse numbers them:
//!   `text` is a `lock::Text`, `toolUse` a `lock::ToolCall` whose input
//!   is one arguments delta, and `reasoningContent` a `lock::Reasoning`
//!   with its `signature` or `redactedContent` as the block's fields. A
//!   stream sends no start for text or reasoning: the first delta opens it.
//! - `stopReason` is the [`Output::Finish`]: `end_turn` and `stop_sequence`
//!   a stop, `tool_use`, `max_tokens`, and `content_filtered` and
//!   `guardrail_intervened` filtered; any other is native.
//! - `usage` is [`Output::Usage`]: the cache reads and writes are parts of the
//!   input.
//! - What else a response says (`metrics`, `trace`, `serviceTier`, the
//!   model's own response fields) is an inert [`Output::Native`]; a block of
//!   another type (a citation, a guard) is not.
//!
//! A failure in the stream (`:message-type` `exception` or `error`) is an
//! upstream fault of its type; a stream that ends before `messageStop`, or
//! inside a message, was cut. The key is a bearer token (a Bedrock API key):
//! requests are not signed with SigV4.

mod decode;
mod encode;
mod read;
mod request;
#[cfg(test)]
mod tests;

use crate::schema::{Asked, Framing, Names, Schema, unwritable_content};
use respan_gateway_conversation::{Assembled, Content, Finish, Input, Output, Role};
use respan_gateway_core::{Connection, Fault, Format, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::header::{CONTENT_TYPE, HeaderName, HeaderValue};
use respan_gateway_http::http::{
    self, request::Parts as RequestHead, response::Parts as ResponseHead,
};
use respan_gateway_http::path::escape;
use respan_gateway_http::{ClientIo, Connect, Encoder, ProviderIo, Upstream};
use serde_json::{Map, Value, json};

use decode::ConverseDecoder;
use request::{split_system, write_config, write_messages, write_tools};

/// This format's name.
const BEDROCK: &str = "bedrock-converse";

const ROLES: Names<Role> = Names(
    &[("user", Role::User), ("assistant", Role::Assistant)],
    BEDROCK,
);

const FINISH: Names<Finish> = Names(
    &[
        ("end_turn", Finish::Stop),
        ("stop_sequence", Finish::Stop),
        ("max_tokens", Finish::Length),
        ("tool_use", Finish::ToolUse),
        ("content_filtered", Finish::Filtered),
    ],
    BEDROCK,
);

/// A reason as a [`Finish`]: a guardrail's intervention filters the answer, a
/// full context window is the token limit.
fn finish_of(reason: &str) -> Finish {
    match reason {
        "guardrail_intervened" => Finish::native_or(BEDROCK, reason, Finish::Filtered),
        "model_context_window_exceeded" => Finish::native_or(BEDROCK, reason, Finish::Length),
        _ => FINISH.finish(reason),
    }
}

/// Amazon Bedrock's Converse, named `bedrock-converse`: the provider's base
/// URL is its region's runtime,
/// `https://bedrock-runtime.{region}.amazonaws.com`, and its transport the
/// key, a Bedrock API key as a bearer token.
#[derive(Debug, Clone, Copy, Default)]
pub struct BedrockConverse;

impl BedrockConverse {
    /// This format's name, `bedrock-converse`.
    pub fn name(&self) -> &'static str {
        BEDROCK
    }
}

/// A bearer key, over HTTP, beside its base URL.
impl Upstream for BedrockConverse {
    fn name(&self) -> &str {
        BEDROCK
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for BedrockConverse {
    fn name(&self) -> &str {
        BEDROCK
    }
    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        self.bind_provider(io)
    }
}

impl Format<ClientIo, Input, Output> for BedrockConverse {
    fn name(&self) -> &str {
        BEDROCK
    }
    fn matches(&self, io: &ClientIo) -> bool {
        self.accepts(io.request())
    }
    fn bind(&self, io: ClientIo) -> Result<Connection<Input, Output>, Fault> {
        self.bind_client(io)
    }
}

/// The media type of ConverseStream's answer.
const EVENT_STREAM: &str = "application/vnd.amazon.eventstream";

/// The header Bedrock names an error's type in, which its SDKs read.
const ERROR_TYPE: HeaderName = HeaderName::from_static("x-amzn-errortype");

/// The model, unescaped, and whether a stream is asked, of a Converse path:
/// `.../model/{model}/converse` or `/converse-stream`.
pub fn call_of(uri: &http::Uri) -> Option<(String, bool)> {
    let mut segments = uri.path().rsplit('/');
    let stream = match segments.next()? {
        "converse" => false,
        "converse-stream" => true,
        _ => return None,
    };
    let model = segments.next().filter(|m| !m.is_empty())?;
    (segments.next() == Some("model")).then(|| {
        let model = percent_encoding::percent_decode_str(model).decode_utf8_lossy();
        (model.into_owned(), stream)
    })
}

/// The type Bedrock names an error of `status` by.
fn error_type(status: http::StatusCode) -> &'static str {
    match status.as_u16() {
        400 | 413 | 422 => "ValidationException",
        401 | 403 => "AccessDeniedException",
        404 => "ResourceNotFoundException",
        408 => "ModelTimeoutException",
        424 => "ModelErrorException",
        429 => "ThrottlingException",
        503 => "ServiceUnavailableException",
        _ => "InternalServerException",
    }
}

impl Schema for BedrockConverse {
    const NAME: &'static str = BEDROCK;
    const PATH: &'static str = "/model/{model}/converse";
    const STREAM_FLAG: bool = false;
    const FRAMING: Framing = Framing::Own("application/vnd.amazon.eventstream");
    type Decoding = ConverseDecoder;

    fn decoder(&self) -> ConverseDecoder {
        ConverseDecoder::default()
    }

    fn decode_body(&self, v: &Value, out: &mut Vec<Output>) -> Result<(), Fault> {
        self.decoder().body(v, out)
    }

    fn accepts<B>(&self, request: &http::Request<B>) -> bool {
        request.method() == http::Method::POST && call_of(request.uri()).is_some()
    }

    fn streams(uri: &http::Uri) -> bool {
        call_of(uri).is_some_and(|(_, stream)| stream)
    }

    /// A client's request: its body, and the model its path names.
    fn read_request(&self, head: &RequestHead) -> Box<dyn respan_gateway_http::Decoder<Input>> {
        let (schema, head) = (*self, head.clone());
        Box::new(move |raw: Bytes, out: &mut Vec<Input>| {
            let v = match serde_json::from_slice::<Value>(&raw) {
                Ok(v) if v.is_object() => v,
                _ => return Err(Fault::bad_request("request body is not a JSON object")),
            };
            let mut chat = schema.decode_json(&v);
            chat.model = call_of(&head.uri).map(|(model, _)| model);
            out.push(chat);
            Ok(())
        })
    }

    /// The client's framing, by its path: ConverseStream's event stream for
    /// a success it streams, else JSON.
    fn response_encoder(
        &self,
        head: &mut ResponseHead,
        request: &http::Request<Bytes>,
    ) -> Box<dyn Encoder<Output>> {
        let stream = Self::streams(request.uri()) && head.status.is_success();
        let media = if stream {
            EVENT_STREAM
        } else {
            "application/json"
        };
        head.headers
            .insert(CONTENT_TYPE, HeaderValue::from_static(media));
        match stream {
            true => self.stream_encoder(Asked::default()),
            false => crate::schema::whole_encoder(*self),
        }
    }

    /// Bedrock's error: `{"message"}`, its type in `x-amzn-errortype`.
    fn refusal(
        &self,
        fault: &Fault,
        _request: &http::Request<Bytes>,
        head: &mut ResponseHead,
    ) -> Bytes {
        let body = crate::schema::refusal_body::<Self>(fault, head);
        let kind = HeaderValue::from_static(error_type(head.status));
        head.headers.insert(ERROR_TYPE, kind);
        body
    }

    fn decode_part(_: &Value) -> Option<Content> {
        None
    }

    fn encode_part(_: &Content) -> Result<Value, Fault> {
        Err(unwritable_content())
    }

    /// What is kept beside a response: its metrics, trace and the model's
    /// own fields.
    fn is_inert(raw: &Value) -> bool {
        matches!(
            raw["type"].as_str(),
            Some("metadata_fields" | "stop_fields")
        )
    }

    fn is_response(v: &Value) -> bool {
        v.get("output").is_some() || v.get("stopReason").is_some()
    }

    /// The model is in the path, not in the body.
    fn encode_request(&self, chat: &Input) -> Result<Map<String, Value>, Fault> {
        let keep = crate::schema::Keep::default();
        let in_the_request = |key: &str| format!("'{key}' in the request");
        let mut native = crate::schema::kept_fields(BEDROCK, &chat.native, in_the_request, keep)?;
        let mut v = self.encode_json(chat)?;
        // The model's own fields beside the reasoning written from the kind.
        let fields = "additionalModelRequestFields";
        if let (Some(Value::Object(own)), Some(Value::Object(written))) =
            (native.shift_remove(fields), v.get_mut(fields))
        {
            for (key, value) in own {
                written.entry(key).or_insert(value);
            }
        }
        v.extend(native);
        Ok(v)
    }

    fn target(&self, input: &Input, stream: bool) -> Result<Option<String>, Fault> {
        let model = input.model.as_deref().filter(|m| !m.is_empty());
        let model = model.ok_or_else(|| {
            Fault::bad_request("a Bedrock call needs a model, such as amazon.nova-lite-v1:0")
        })?;
        let method = if stream {
            "converse-stream"
        } else {
            "converse"
        };
        Ok(Some(format!("/model/{}/{method}", escape(model))))
    }

    fn encode_json(&self, chat: &Input) -> Result<Map<String, Value>, Fault> {
        let (system, rest) = split_system(chat)?;
        let messages = write_messages(rest)?;
        if messages.is_empty() {
            return Err(Fault::bad_request(
                "Bedrock needs at least one user or assistant message",
            ));
        }
        let mut v = Map::new();
        v.insert("messages".into(), messages.into());
        if !system.is_empty() {
            v.insert("system".into(), system.into());
        }
        write_tools(chat, &mut v)?;
        write_config(chat, &mut v)?;
        Ok(v)
    }

    fn decode_json(&self, v: &Value) -> Input {
        read::read(v)
    }

    fn encode_whole(&self, whole: &Assembled) -> Result<Value, Fault> {
        encode::whole(whole)
    }

    fn stream_encoder(&self, _: Asked) -> Box<dyn Encoder<Output>> {
        Box::new(encode::EventStream::default())
    }

    /// Bedrock's error body: `{"message": ..}`.
    fn error_body(fault: &Fault) -> Value {
        json!({ "message": fault.message() })
    }
}
