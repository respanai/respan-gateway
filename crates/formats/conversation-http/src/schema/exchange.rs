//! The HTTP exchange of a conversation format: requests posted, responses
//! read into events with a hold of their start, and events encoded into a
//! client's response, streamed or as one whole body.
//!
//! Nothing here reuses the bytes a value was decoded from: values are plain
//! data, and an encoder writes what a value says. A call to a provider of
//! the client's own format carries the client's spec headers and query
//! parameters ([`Schema::CARRIED`]).

use respan_gateway_core::FaultKind;
use std::sync::Arc;

use respan_gateway_conversation::Usage;
use respan_gateway_conversation::{Assembled, Input, Output};
use respan_gateway_core::Fault;
use respan_gateway_http::bytes::{BufMut, Bytes, BytesMut};
use respan_gateway_http::content::{content_type, is_stream};
use respan_gateway_http::http::{self, HeaderValue, StatusCode, header::CONTENT_TYPE};
use respan_gateway_http::{
    Decoder, Encoder, Sse, Whole,
    http::request::Parts as RequestHead,
    http::response::Parts as ResponseHead,
    raw::{Original, RawReply},
    rules::bodiless,
};
use respan_gateway_sse::event_data;
use serde_json::{Map, Value};

use super::errors::{answer, failure_json, said_error, says_error};
use super::values::{shows, unwritable_content};
use super::{Framing, Schema, StreamResponse};

/// The call of `input` in `schema`'s format, a JSON `POST` to
/// [`Schema::PATH`] with the format's headers ([`Schema::HEADERS`]), and its
/// encoder ([`RequestEncoder`]). It asks for a stream as the call's
/// [`StreamResponse`] says, else as the client's original request
/// ([`Original`]) did, else for one. The client's request, if of this format,
/// carries its spec's headers and query parameters ([`Schema::CARRIED`],
/// [`Schema::CARRIED_QUERY`]) and gives its spelling of a field the format
/// spells two ways ([`Schema::respell`]). `Err` with `config` for live input.
pub(crate) fn post<C: Schema>(
    schema: C,
    head: &mut RequestHead,
    input: Option<&Input>,
    original: Option<&Original>,
    stream: Option<StreamResponse>,
) -> Result<Box<dyn Encoder<Input>>, Fault> {
    if input.is_none() {
        return Err(Fault::config(format!(
            "{} does not accept streaming inputs",
            C::NAME
        )));
    }
    head.method = http::Method::POST;
    head.headers
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    // The client's request, if of this format: its spec's headers and query
    // parameters go on, and it spells what the format spells two ways.
    let own = own_request::<C>(original);
    if let Some(own) = own {
        for name in C::CARRIED {
            for value in own.headers().get_all(*name) {
                head.headers.append(*name, value.clone());
            }
        }
    }
    for (name, value) in C::HEADERS {
        let value = HeaderValue::from_static(value);
        head.headers.entry(*name).or_insert(value);
    }
    let stream = stream.map(|s| s.0).unwrap_or_else(|| {
        let asked = original.and_then(|w| w.memo(|r| probe(r.body())));
        asked.is_none_or(|asked| asked.stream)
    });
    match input.and_then(|input| schema.target(input, stream).transpose()) {
        Some(target) => {
            let target = target?;
            head.uri = target.parse().map_err(|e| {
                Fault::internal(format!("a call's path {target:?} is no path")).with_source(e)
            })?;
        }
        None => respan_gateway_http::path::set_path(head, C::PATH)?,
    }
    if let Some(query) = own.and_then(|r| carried_query::<C>(r.uri())) {
        let at = match head.uri.query() {
            Some(own) => format!("{}?{own}&{query}", head.uri.path()),
            None => format!("{}?{query}", head.uri.path()),
        };
        head.uri = at.parse().map_err(|e| {
            Fault::internal(format!("a call's path {at:?} is no path")).with_source(e)
        })?;
    }
    let original = own.map(|r| serde_json::from_slice(r.body()).unwrap_or(Value::Null));
    Ok(Box::new(RequestEncoder {
        schema,
        stream,
        original,
    }))
}

/// The client's request, if of `C`'s format: one to a path ending in
/// [`Schema::PATH`].
pub(crate) fn own_request<C: Schema>(original: Option<&Original>) -> Option<&http::Request<Bytes>> {
    original
        .and_then(Original::get)
        .filter(|r| r.uri().path().ends_with(C::PATH))
}

/// Writes a conversation as a request, asking for a stream if `stream`, in
/// the spelling of the client's `original` request, then what the format
/// adds ([`Schema::finish`]). What cannot be written is the client's fault:
/// without the bytes it came as, the gateway cannot tell an edit from the
/// request.
struct RequestEncoder<C> {
    schema: C,
    stream: bool,
    original: Option<Value>,
}

impl<C: Schema> RequestEncoder<C> {
    pub(crate) fn encode_into(&mut self, chat: &Input, out: &mut BytesMut) -> Result<(), Fault> {
        let mut v =
            self.schema
                .encode_request(chat)
                .map_err(|f| match f.is(FaultKind::Client) {
                    true => f,
                    false => Fault::bad_request(f.message()).with_source(f),
                })?;
        if self.stream && C::STREAM_FLAG {
            v.insert("stream".into(), true.into());
        }
        match &self.original {
            Some(original) => C::respell(original, &mut v),
            None => C::translated(&mut v),
        }
        C::prepare_call(&mut v);
        out.extend_from_slice(&to_bytes(&v)?);
        Ok(())
    }
}

impl<C: Schema> Encoder<Input> for RequestEncoder<C> {
    fn encode(&mut self, chat: &Input, frames: &mut Vec<Bytes>) -> Result<(), Fault> {
        crate::schema::frame(frames, |out| self.encode_into(chat, out))
    }
}

/// The parameters of `uri`'s query that `C`'s spec names
/// ([`Schema::CARRIED_QUERY`]), as they were spelled; `None` if none.
fn carried_query<C: Schema>(uri: &http::Uri) -> Option<String> {
    let kept: Vec<&str> = uri
        .query()?
        .split('&')
        .filter(|p| {
            let name = p.split_once('=').map_or(*p, |(name, _)| name);
            C::CARRIED_QUERY.contains(&name)
        })
        .collect();
    (!kept.is_empty()).then(|| kept.join("&"))
}

/// `v` as JSON bytes.
fn to_bytes(v: &Map<String, Value>) -> Result<Bytes, Fault> {
    let mut out = BytesMut::new().writer();
    let written = serde_json::to_writer(&mut out, v);
    written.map_err(|e| Fault::internal("a request could not be written").with_source(e))?;
    Ok(out.into_inner().freeze())
}

/// What a JSON request asks of its response: a stream, and usage in it.
#[derive(Debug, Default, Clone, Copy)]
pub struct Asked {
    /// Whether it asks for a stream.
    pub stream: bool,
    /// Whether it asks for usage in the stream.
    pub usage: bool,
}

/// What a client's `request` asks of its response: a stream by the body's
/// `stream`, or by the path if the client's format says so there
/// ([`Schema::streams`]).
pub(crate) fn asked<C: Schema>(request: &http::Request<Bytes>) -> Asked {
    let mut asked = probe(request.body());
    asked.stream |= C::streams(request.uri());
    asked
}

/// Keep what the client of format `C` asked of its response with its
/// request's original, for the provider's edge to read.
pub(crate) fn seed_asked<C: Schema>(original: &Original) {
    original.memo(asked::<C>);
}

/// [`Asked`] of a JSON body, reading only its two fields.
pub(crate) fn probe(raw: &[u8]) -> Asked {
    #[derive(serde::Deserialize)]
    struct Probe {
        #[serde(default)]
        stream: Value,
        #[serde(default)]
        stream_options: Value,
    }
    match serde_json::from_slice::<Probe>(raw) {
        Ok(p) => Asked {
            stream: p.stream == true,
            usage: p.stream_options["include_usage"] == true,
        },
        Err(_) => Asked::default(),
    }
}

/// The decoder of a response of `schema`'s format, of `head`, to a call of
/// `method`: none if it has no body, else one of a stream's events, else
/// one of a whole JSON message, which reads a failure's as the provider's
/// error ([`answer`]).
pub(crate) fn read_response<C: Schema>(
    schema: C,
    head: &ResponseHead,
    method: &http::Method,
) -> Option<Box<dyn Decoder<Output>>> {
    if head.status.is_success() && bodiless(method, head.status) {
        return None;
    }
    if streamed::<C>(head) {
        return Some(frames(schema));
    }
    let head = head.clone();
    Some(Box::new(Whole::new(
        move |raw: Bytes, out: &mut Vec<Output>| {
            if !head.status.is_success() {
                let answer = http::Response::from_parts(head.clone(), raw);
                return Err(super::errors::answer(&schema, answer));
            }
            out.extend(unary_reply(schema, head.status, raw)?);
            Ok(())
        },
    )))
}

/// Whether a response of `C`'s format is a stream: a success of the media
/// type its framing names.
fn streamed<C: Schema>(head: &ResponseHead) -> bool {
    match C::FRAMING {
        Framing::Sse => is_stream(head),
        Framing::Own(media) => {
            let essence = content_type(head).and_then(|c| c.split(';').next());
            let own = essence.is_some_and(|e| e.trim().eq_ignore_ascii_case(media));
            own && head.status.is_success()
        }
    }
}

/// The decoder of one streamed response, as its format frames it, its
/// start held ([`Held`]).
fn frames<C: Schema>(schema: C) -> Box<dyn Decoder<Output>> {
    match C::FRAMING {
        Framing::Sse => Box::new(Sse::new(Held::new(schema.decoder()))),
        Framing::Own(_) => Box::new(Held::new(schema.decoder())),
    }
}

/// [`read_response`] of a finite answer, synchronously.
pub(crate) fn decode_response<C: Schema>(
    schema: C,
    response: http::Response<Bytes>,
    call: &RequestHead,
) -> Result<Vec<Output>, Fault> {
    let (head, bytes) = response.into_parts();
    if head.status.is_success() && bodiless(&call.method, head.status) {
        return Ok(Vec::new());
    }
    if streamed::<C>(&head) {
        let mut events = frames(schema);
        let mut out = Vec::new();
        events.decode(bytes, &mut out)?;
        events.end(Ok(()), &mut out)?;
        return Ok(out);
    }
    if !head.status.is_success() {
        return Err(answer(&schema, http::Response::from_parts(head, bytes)));
    }
    unary_reply(schema, head.status, bytes)
}

/// One JSON message, decoded whole: the events of a success, else the
/// provider's error.
fn unary_reply<C: Schema>(schema: C, status: StatusCode, raw: Bytes) -> Result<Vec<Output>, Fault> {
    let v = serde_json::from_slice::<Value>(&raw).ok();
    if let Some(v) = v.as_ref().filter(|v| C::is_response(v)) {
        let mut out = Vec::new();
        schema.decode_body(v, &mut out)?;
        return Ok(out);
    }
    Err(match failure_json(&raw).filter(says_error) {
        Some(v) => said_error(status, raw, &v),
        None => Fault::upstream("the provider's success is not a response of its format"),
    })
}

/// Choose response framing from the explicitly selected head and the
/// client's request, and its encoder: a stream's, as the client [`Asked`],
/// or one whole body's.
pub(crate) fn response_encoder<C: Schema>(
    schema: C,
    head: &mut ResponseHead,
    request: &http::Request<Bytes>,
) -> Box<dyn Encoder<Output>> {
    let asked = asked::<C>(request);
    // A success head's framing is the client's, whatever the provider's was.
    if is_stream(head) != asked.stream {
        head.headers.remove(CONTENT_TYPE);
    }
    if content_type(head).is_none() {
        let content_type = match asked.stream {
            true => "text/event-stream",
            false => "application/json",
        };
        head.headers
            .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    }
    match is_stream(head) {
        true => schema.stream_encoder(asked),
        false => Box::new(UnaryEncoder {
            whole: Assembled::default(),
            schema,
        }),
    }
}

/// A refusal in format `C`: its own words as this format's JSON error.
/// The format has no error under a success, so a refusal under a status
/// that says none is a plain 500, as the adapter writes what no format can.
pub(crate) fn refusal<C: Schema>(fault: &Fault, head: &mut ResponseHead) -> Bytes {
    if !(head.status.is_client_error() || head.status.is_server_error()) {
        head.status = StatusCode::INTERNAL_SERVER_ERROR;
        head.headers.clear();
        return respan_gateway_http::rules::plain_text(head, fault.message());
    }
    let json = HeaderValue::from_static("application/json");
    head.headers.insert(CONTENT_TYPE, json);
    match fault.get::<RawReply>().map(|r| r.0.body()) {
        Some(words) => words.clone(),
        None => C::error_body(fault).to_string().into(),
    }
}

/// Writes one message at its end: the events collected into a whole body. A
/// failure, before any byte, is refused.
struct UnaryEncoder<C> {
    whole: Assembled,
    schema: C,
}

impl<C: Schema> UnaryEncoder<C> {
    pub(crate) fn encode_into(&mut self, reply: &Output, _out: &mut BytesMut) -> Result<(), Fault> {
        self.whole.push(reply).map_err(|_| unwritable_content())
    }

    pub(crate) fn end_into(
        &mut self,
        end: &Result<(), Fault>,
        out: &mut BytesMut,
    ) -> Result<(), Fault> {
        end.clone()?;
        let body = self.schema.encode_whole(&self.whole)?;
        out.extend_from_slice(body.to_string().as_bytes());
        Ok(())
    }
}

impl<C: Schema> Encoder<Output> for UnaryEncoder<C> {
    fn encode(&mut self, reply: &Output, frames: &mut Vec<Bytes>) -> Result<(), Fault> {
        crate::schema::frame(frames, |out| self.encode_into(reply, out))
    }

    fn end(&mut self, end: Result<(), Fault>, frames: &mut Vec<Bytes>) -> Result<(), Fault> {
        crate::schema::frame(frames, |out| self.end_into(&end, out))
    }
}

/// What an encoder of a stream has written but keeps back until a reply shows
/// content ([`shows`]), so that a failure before it leaves nothing written
/// and the response is refused, as the decoder holds a start for.
#[derive(Debug, Default)]
pub(crate) struct Gate {
    shown: bool,
    held: BytesMut,
}

impl Gate {
    /// Let through, with what came before, what `reply` made, if it or any
    /// before it shows content.
    pub fn pass(&mut self, reply: &Output, made: &[u8], out: &mut BytesMut) {
        self.held.extend_from_slice(made);
        if self.shown || shows(reply) {
            self.release(out);
        }
    }

    /// Let through all that is held: a stream that ended well shows what it
    /// has.
    pub fn release(&mut self, out: &mut BytesMut) {
        self.shown = true;
        out.extend_from_slice(&self.held.split());
    }
}

/// A decoder of a streamed response's events that keeps its start, and
/// what comes before its first content, back until that content comes, so a
/// failure before it leaves nothing started. A failure keeps the usage held.
#[derive(Debug)]
pub(crate) struct Held<D> {
    reading: D,
    held: Vec<Output>,
    through: bool,
}

impl<D> Held<D> {
    /// The events `reading` decodes, their start held.
    pub fn new(reading: D) -> Self {
        Self {
            reading,
            held: Vec::new(),
            through: false,
        }
    }
}

impl<D: Decoder<Output>> Decoder<Output> for Held<D> {
    fn decode(&mut self, raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
        let from = out.len();
        let result = self.reading.decode(raw, out);
        self.hold(out, from, result.is_err());
        result
    }

    /// A broken stream ends at once; one that ended well gives what its
    /// reader holds, then all that is held.
    fn end(&mut self, end: Result<(), Fault>, out: &mut Vec<Output>) -> Result<(), Fault> {
        end?;
        let from = out.len();
        let result = self.reading.end(Ok(()), out);
        self.hold(out, from, result.is_err());
        out.append(&mut self.held);
        result
    }

    fn limit(&mut self, bytes: Option<usize>) {
        self.reading.limit(bytes);
    }
}

impl<D> Held<D> {
    fn hold(&mut self, out: &mut Vec<Output>, from: usize, failed: bool) {
        for reply in out.split_off(from) {
            match reply {
                _ if self.through => out.push(reply),
                ref r if !r.is_content() && self.held.is_empty() => out.push(reply),
                ref r if !r.is_content() || !shows(r) => self.held.push(reply),
                _ => {
                    out.append(&mut self.held);
                    out.push(reply);
                    self.through = true;
                }
            }
        }
        if failed && !self.through {
            let mut usage = Usage::default();
            for held in self.held.drain(..) {
                if let Output::Usage(u) = held {
                    usage += &u;
                }
            }
            if !usage.is_empty() {
                out.push(Output::Usage(usage));
            }
        }
    }
}

/// The events of a stream event that is not its format's JSON, as an
/// [`Output::Native`] of `format` so filters read it; one with no data, such
/// as a comment, says nothing.
pub(crate) fn unreadable_event(format: &Arc<str>, raw: &Bytes, out: &mut Vec<Output>) {
    if let Some(data) = event_data(raw) {
        out.push(Output::native(format.clone(), data));
    }
}

/// The fault of a provider's error event `v`, in a response of format
/// `name`, whose data goes as it came to a client of this format.
pub(crate) fn error_event(raw: &[u8], v: &Value) -> Fault {
    let data = Bytes::from(event_data(raw).unwrap_or_default());
    said_error(StatusCode::OK, data, v)
}
