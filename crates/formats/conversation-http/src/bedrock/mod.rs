//! Amazon Bedrock's Converse: `POST /model/{modelId}/converse` and
//! `/converse-stream`, of the conversation kind, as a [`Schema`]
//! ([`BedrockConverse`]), at the provider edge only: Bedrock serves every
//! model it hosts on Converse, and no client speaks it to the gateway.
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
//! system block with a cache marker (Anthropic's `cache_control`) is
//! followed by a `cachePoint`. A part is a `text`,
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
//!   model's own response fields), and a block of another type (a citation,
//!   a guard), is an [`Output::Native`], which another format drops.
//!
//! A failure in the stream (`:message-type` `exception` or `error`) is an
//! upstream fault of its type; a stream that ends before `messageStop`, or
//! inside a message, was cut. The key is a bearer token (a Bedrock API key).

mod decode;
mod request;
#[cfg(test)]
mod tests;

use crate::schema::{Asked, Framing, Names, Schema, native_fields, rest_of, unwritable_content};
use respan_gateway_conversation::{Assembled, Content, Finish, Input, Output, Role};
use respan_gateway_core::{Connection, Fault, Format, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::path::escape;
use respan_gateway_http::{Connect, Encoder, ProviderIo, Upstream};
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

/// The image formats Converse takes, by media type.
const IMAGES: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
];

/// The document formats Converse takes, by media type.
const DOCUMENTS: &[(&str, &str)] = &[
    ("pdf", "application/pdf"),
    ("csv", "text/csv"),
    ("doc", "application/msword"),
    (
        "docx",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    ),
    ("xls", "application/vnd.ms-excel"),
    (
        "xlsx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    ),
    ("html", "text/html"),
    ("txt", "text/plain"),
    ("md", "text/markdown"),
];

/// The Converse format of a media type in `table`.
fn format_of(table: &[(&'static str, &str)], mime: &str) -> Option<&'static str> {
    table.iter().find(|(_, m)| *m == mime).map(|(f, _)| *f)
}

/// The media type of ConverseStream's answer.
const EVENT_STREAM: &str = "application/vnd.amazon.eventstream";

impl Schema for BedrockConverse {
    const NAME: &'static str = BEDROCK;
    const PATH: &'static str = "/model/{model}/converse";
    const STREAM_FLAG: bool = false;
    const FRAMING: Framing = Framing::Own(EVENT_STREAM);
    type Decoding = ConverseDecoder;

    fn decoder(&self) -> ConverseDecoder {
        ConverseDecoder::default()
    }

    fn decode_body(&self, v: &Value, out: &mut Vec<Output>) -> Result<(), Fault> {
        self.decoder().body(v, out)
    }

    fn decode_part(_: &Value) -> Option<Content> {
        None
    }

    fn encode_part(_: &Content) -> Result<Value, Fault> {
        Err(unwritable_content())
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

    /// Not read: the request stays native, so only Bedrock is sent it.
    fn decode_json(&self, v: &Value) -> Input {
        let mut chat = Input::default();
        chat.native = native_fields(BEDROCK, rest_of(v, &[]));
        chat
    }

    fn encode_whole(&self, _: &Assembled) -> Result<Value, Fault> {
        Err(no_client())
    }

    fn stream_encoder(&self, _: Asked) -> Box<dyn Encoder<Output>> {
        Box::new(Unwritten)
    }

    /// Bedrock's error body: `{"message": ..}`.
    fn error_body(fault: &Fault) -> Value {
        json!({ "message": fault.message() })
    }
}

/// What a client would be answered in: none, as no client speaks Converse.
fn no_client() -> Fault {
    Fault::internal("Converse answers no client")
}

/// The encoder of an answer to a client, which Converse has none of.
struct Unwritten;

impl Encoder<Output> for Unwritten {
    fn encode(&mut self, _: &Output, _: &mut Vec<Bytes>) -> Result<(), Fault> {
        Err(no_client())
    }
}
