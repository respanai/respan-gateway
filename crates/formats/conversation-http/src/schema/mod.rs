//! The conversation kind's binding to HTTP, which its formats share. A
//! vendor supplies its [`Schema`]: its names and its JSON, both ways; the
//! HTTP around it, on either edge, is the trait's provided methods. Wire
//! helpers live in [`respan_gateway_http::content`].
//!
//! Each format decodes a request into an [`Input`], keeping the fields the
//! kind has no place for in its `native`, and writes a request from the
//! value alone. A response, streamed or whole, decodes into the events of
//! [`Output`] and is encoded from them, so what survives a relay is what
//! the events and `native` say, not the provider's bytes. A request to a
//! provider of its own format keeps the headers and query parameters of
//! the format's spec ([`Schema::CARRIED`]).
//!
//! What a format has no form for stays in its own terms (optional
//! `NativeFields`, a `Content::Native`, a `Output::Native`) and is written
//! only by that format; another drops it with a warning. A request that asks for several choices (`n > 1`) cannot be
//! written and is refused.

mod errors;
mod exchange;
mod values;

#[cfg(test)]
mod tests;

pub(crate) use crate::fields::{rest_of, unix_now};
pub use errors::{merge_details, read_provider_error};
pub(crate) use errors::{says, says_error};
pub use exchange::Asked;
#[cfg(test)]
pub(crate) use exchange::Held;
pub(crate) use exchange::{Gate, error_event, response_encoder, unreadable_event};
pub(crate) use respan_gateway_conversation::block_fields;
pub(crate) use values::parts_within_totals;
pub(crate) use values::{
    Names, NativeFieldsExt, Totals, UsageFields, arguments_object, check_usage, count,
    decode_content, decode_message, decode_tools, each_tool, encode_content, encode_message, ended,
    fits, merge_fields, merge_into, merge_missing, native_fields, only, own_fields, own_shapes,
    read_search, remainder, set, source_url, stop_json, take_strict, take_text, tidy,
    unsupported_fields, untranslatable, unwritable_content, usage_extra, warn_reasoning,
    write_search,
};
pub(crate) use values::{effort_name, own_native, parse_effort, reasoning_of, take_native};

use respan_gateway_conversation::{Assembled, Content, Input, Output};
use respan_gateway_core::{Connection, EventSink, Fault};
use respan_gateway_http::bytes::{Bytes, BytesMut};
use respan_gateway_http::http::{
    self, StatusCode, request::Parts as RequestHead, response::Parts as ResponseHead,
};
use respan_gateway_http::{ClientIo, Encoder, ProviderIo, Reply, rules::bodiless};
use serde_json::{Map, Value};

/// Provider HTTP delivery preference, set in call extensions.
/// Without an override the HTTP binding follows the original client request,
/// or requests a stream when there is no original HTTP request.
/// Standalone semantic request encoding produces a finite request.
/// Client response framing remains determined by the client request.
#[derive(Debug, Clone, Copy)]
pub struct StreamResponse(pub bool);

/// How a format's streamed response is framed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// Server-sent events (`text/event-stream`), one event at a time to the
    /// format's decoder.
    Sse,
    /// A body of this media type, whose bytes the format's decoder frames
    /// itself, such as AWS's event stream.
    Own(&'static str),
}

/// A vendor's JSON schema for the conversation kind: what a conversation
/// format supplies. The HTTP binding around it, at both edges, is shared:
/// these provided methods, which a format's `Format` implementations call
/// ([`Schema::bind_client`], [`Schema::bind_provider`]).
///
/// The verbs say what a method does: `decode_*` and `encode_*` convert
/// finite values synchronously (JSON, a part, a whole body), and `read_*`
/// and `write_*` bind lazy streams of events to an HTTP body.
pub trait Schema: Copy + Send + Sync + 'static {
    /// The wire format's name, which tags what its values hold that the
    /// kind has no place for (`NativeFields`, `Content::Native`,
    /// `Output::Native`, a native role or finish).
    const NAME: &'static str;
    /// The path a request is posted to, which a client's request ends with.
    const PATH: &'static str;
    /// Whether a request that asks for a stream says so in its body
    /// (`"stream": true`), as OpenAI's and Anthropic's do; a format that says
    /// it in the path leaves it out.
    const STREAM_FLAG: bool = true;
    /// How a streamed response is framed, which its media type says.
    const FRAMING: Framing = Framing::Sse;
    /// Headers a request to a provider gets if it has none of that name.
    const HEADERS: &'static [(&'static str, &'static str)] = &[];
    /// The request headers of this format's spec, such as a version or beta
    /// flags: a client's request of this format carries them to a provider
    /// of it, where they replace [`Schema::HEADERS`].
    const CARRIED: &'static [&'static str] = &[];
    /// The query parameters of this format's spec, carried as
    /// [`Schema::CARRIED`] is.
    const CARRIED_QUERY: &'static [&'static str] = &[];
    /// The keys of a conversation's `native` this format writes itself, from
    /// the conversation and the native together.
    const OWN: &'static [&'static str] = &[];
    /// Decodes the events of a streamed response into events.
    type Decoding: respan_gateway_http::Decoder<Output>;

    /// A decoder of one streamed response's events.
    fn decoder(&self) -> Self::Decoding;
    /// The events of one whole response body `v`.
    fn decode_body(&self, v: &Value, out: &mut Vec<Output>) -> Result<(), Fault>;
    /// A JSON request as a conversation, noting what it has no place for.
    fn decode_json(&self, v: &Value) -> Input;
    /// A conversation as a request, but for its own `native` fields and the
    /// model, refusing what the kind says that this format cannot.
    fn encode_json(&self, chat: &Input) -> Result<Map<String, Value>, Fault>;
    /// A message part the kind has a form for beyond text, such as media or
    /// a tool block; `None` keeps the part this format's own.
    fn decode_part(p: &Value) -> Option<Content>;
    /// A part beyond text and this format's own parts, as it writes it, or
    /// refused.
    fn encode_part(c: &Content) -> Result<Value, Fault>;
    /// Whether a successful body is a whole response of this format.
    fn is_response(v: &Value) -> bool;
    /// The events of a response, collected, as one whole body.
    fn encode_whole(&self, whole: &Assembled) -> Result<Value, Fault>;
    /// The encoder of a stream to a client that [`Asked`] so.
    fn stream_encoder(&self, asked: Asked) -> Box<dyn Encoder<Output>>;
    /// The error body for `fault`, its details merged by [`merge_details`].
    fn error_body(fault: &Fault) -> Value;
    /// The status HTTP names for what a provider's error answer of `status`
    /// says: `status` itself, unless the vendor uses one of its own.
    fn status(status: StatusCode) -> StatusCode {
        status
    }
    /// Prepare the request sent to a provider: add what the gateway needs of it,
    /// such as usage in a stream.
    fn prepare_call(_v: &mut Map<String, Value>) {}
    /// Prepare a call of `input` whose client speaks another format: say
    /// what the format must for a client that can name nothing it keeps.
    fn translated(_input: &Input, _v: &mut Map<String, Value>) {}
    /// Spell the fields of `v` as `original`, the client's request in this
    /// format, did, where the format has two spellings of one field.
    fn respell(_original: &Value, _v: &mut Map<String, Value>) {}
    /// The path and query of the call for `input`, if they depend on it or on
    /// whether a stream is asked, such as a model named in the path; `None`
    /// for [`Schema::PATH`], keeping the call's query.
    fn target(&self, _input: &Input, _stream: bool) -> Result<Option<String>, Fault> {
        Ok(None)
    }
    /// Whether the call at `uri` asks for a stream by its path, as a format
    /// does that names its method there; else the request body says.
    fn streams(_uri: &http::Uri) -> bool {
        false
    }

    /// Whether a client's request is of this format: a `POST` to a path
    /// ending in [`Schema::PATH`].
    fn accepts<B>(&self, request: &http::Request<B>) -> bool {
        request.method() == http::Method::POST
            && respan_gateway_http::path::path_ends_with(request.uri(), Self::PATH)
    }

    /// The decoder of a client's request of `head`, one JSON body.
    fn read_request(&self, _head: &RequestHead) -> Box<dyn respan_gateway_http::Decoder<Input>> {
        let schema = *self;
        Box::new(move |raw: Bytes, out: &mut Vec<Input>| {
            out.push(schema.decode_request(raw)?);
            Ok(())
        })
    }

    /// Decode a finite JSON request synchronously.
    fn decode_request(&self, raw: Bytes) -> Result<Input, Fault> {
        let v = match serde_json::from_slice::<Value>(&raw) {
            Ok(v) if v.is_object() => v,
            _ => return Err(values::not_an_object()),
        };
        let mut chat = self.decode_json(&v);
        chat.model = v["model"].as_str().map(Into::into);
        Ok(chat)
    }

    /// A conversation as a request's JSON body: its fields the kind has no
    /// place for are written only if they are this format's.
    fn encode_request(&self, chat: &Input) -> Result<Map<String, Value>, Fault> {
        let chat = &*own_shapes(chat, Self::NAME);
        let mut native = chat.native.fields_for(Self::NAME);
        native.retain(|key, _| !Self::OWN.contains(&key.as_str()));
        let mut v = Map::new();
        set(&mut v, "model", chat.model.clone().map(Into::into));
        v.extend(self.encode_json(chat)?);
        v.extend(native);
        Ok(v)
    }

    /// The decoder of a provider's answer of `head` to the `call`, with
    /// fresh state; `None` if it has no body.
    fn read_response(
        &self,
        head: &ResponseHead,
        call: &http::Request<Bytes>,
    ) -> Option<Box<dyn respan_gateway_http::Decoder<Output>>> {
        exchange::read_response(*self, head, call.method())
    }

    /// Decode a finite answer synchronously, without polling an async stream.
    fn decode_response(
        &self,
        response: http::Response<Bytes>,
        call: &RequestHead,
    ) -> Result<Vec<Output>, Fault> {
        exchange::decode_response(*self, response, call)
    }

    /// Select response framing and its encoder: the client's, whatever the
    /// provider's was.
    fn response_encoder(
        &self,
        head: &mut ResponseHead,
        request: &http::Request<Bytes>,
    ) -> Box<dyn Encoder<Output>> {
        exchange::response_encoder(*self, head, request)
    }

    /// Bind response delivery, its head the one a plugin chose, else a
    /// success's: the encoder of a client's response.
    fn write_response(&self, reply: Reply) -> Result<EventSink<Output>, Fault> {
        let mut head = reply.head();
        let encoder = self.response_encoder(&mut head, reply.request());
        Ok(reply.write(head, encoder))
    }

    /// Render an HTTP refusal in this format's error schema: the error
    /// writer of a client's response.
    fn refusal(
        &self,
        fault: &Fault,
        _request: &http::Request<Bytes>,
        head: &mut ResponseHead,
    ) -> Bytes {
        exchange::refusal::<Self>(fault, head)
    }

    /// Encode a finite answer synchronously, including finalization errors;
    /// one HTTP gives no body ([`bodiless`]) has none.
    fn encode_response(
        &self,
        request: &http::Request<Bytes>,
        replies: &[Output],
    ) -> Result<http::Response<Bytes>, Fault> {
        let mut head = http::Response::new(()).into_parts().0;
        let encoder = self.response_encoder(&mut head, request);
        let bytes = match bodiless(request.method(), head.status) {
            true => Bytes::new(),
            false => {
                let mut encoder = encoder;
                encoder.encode_all(replies)?
            }
        };
        Ok(http::Response::from_parts(head, bytes))
    }

    /// This format at the client edge: requests in, responses out.
    fn bind_client(&self, io: ClientIo) -> Result<Connection<Input, Output>, Fault> {
        let schema = *self;
        let original = io
            .extensions()
            .get::<respan_gateway_http::raw::Original>()
            .cloned();
        let io = io.with_error(move |fault, request, head| schema.refusal(fault, request, head));
        Ok(io.bind(
            move |head| {
                if let Some(original) = &original {
                    exchange::seed_asked::<Self>(original);
                }
                schema.read_request(head)
            },
            move |reply| schema.write_response(reply),
        ))
    }

    /// This format at the provider edge: a call out, its response in, as the
    /// call's [`StreamResponse`] and the client's request ask.
    fn bind_provider(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        let schema = *self;
        let stream = io.extensions().get::<StreamResponse>().copied();
        let original = io
            .extensions()
            .get::<respan_gateway_http::raw::Original>()
            .cloned();
        let io = io.with_refusal(move |answer| errors::answer(&schema, answer));
        Ok(io.bind(
            move |head, input| exchange::post(schema, head, input, original.as_ref(), stream),
            move |head, call| schema.read_response(head, call),
        ))
    }
}

/// What `write` writes into one buffer, as one frame if it wrote anything:
/// how an encoder of one body's chunks gives them as frames.
pub(crate) fn frame(
    frames: &mut Vec<Bytes>,
    write: impl FnOnce(&mut BytesMut) -> Result<(), Fault>,
) -> Result<(), Fault> {
    let mut out = BytesMut::new();
    write(&mut out)?;
    if !out.is_empty() {
        frames.push(out.freeze());
    }
    Ok(())
}
