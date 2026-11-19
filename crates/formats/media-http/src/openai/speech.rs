//! OpenAI Audio Speech: `POST /audio/speech` (JSON), answered with the
//! audio's bytes or, if asked (`stream_format: "sse"`), a stream of events.

use super::{
    Fields, Form, as_usage_extras, b64, event_json, invalid, json_bytes, merge_missing, missing,
    post_to, read_usage, respond_client, sends, set, sse_event, streams, take_number, take_string,
    unb64, unreadable, usage_extras, write_usage,
};
use crate::edge::{bind_provider, ended_events};
use futures::{Sink, ready};
use respan_gateway_conversation::Usage;
use respan_gateway_core::Provider;
use respan_gateway_core::{Connection, EventSink, Fault, Format, Native};
use respan_gateway_http::bytes::{Bytes, BytesMut};
use respan_gateway_http::content;
use respan_gateway_http::http::{self, Method, request::Parts as RequestHead};
use respan_gateway_http::path::path_ends_with;
use respan_gateway_http::{ClientIo, Decoder, Encoder, ProviderIo, Reply, native::Json};
use respan_gateway_http::{Connect, Upstream};
use respan_gateway_media::speech::{Input, Output};
use serde_json::{Map, Value};
use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};

const NAME: &str = "openai-speech";

type Key = crate::Own<OpenAiSpeech>;
const SPEECH: &str = "/audio/speech";

/// OpenAI Audio Speech, as OpenAI or a compatible vendor serves it: a
/// `POST` to a path ending in `/audio/speech`.
///
/// The audio's bytes decode into [`Output::Audio`] chunks as they come, and
/// are relayed so, the provider read no faster than the client takes them.
/// A stream of events decodes into the same chunks, from each
/// `speech.audio.delta`, and the usage of its `speech.audio.done`, failing
/// `truncated` without one. A client is answered in the framing it asked
/// for: the audio's bytes, labelled as their first bytes say, else as its
/// `response_format` names, or a stream of events.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenAiSpeech;

impl OpenAiSpeech {
    /// The format's name, `openai-speech`.
    pub const NAME: &'static str = NAME;
}

impl Format<ClientIo, Input, Output> for OpenAiSpeech {
    fn name(&self) -> &str {
        NAME
    }

    fn matches(&self, io: &ClientIo) -> bool {
        let request = io.request();
        request.method() == Method::POST && path_ends_with(request.uri(), SPEECH)
    }

    fn bind(&self, io: ClientIo) -> Result<Connection<Input, Output>, Fault> {
        Ok(respond_client(io, decode_request, respond))
    }
}

/// Its spec's: a bearer key, over HTTP, beside its base URL.
impl Upstream for OpenAiSpeech {
    fn name(&self) -> &str {
        NAME
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for OpenAiSpeech {
    fn name(&self) -> &str {
        NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        let stream = streams(&io, &[SPEECH], asks_events);
        Ok(bind_provider(
            io,
            NAME,
            move |head, input| post(head, input, stream),
            |head, _| Some(read_response(head)),
        ))
    }
}

/// Whether a request asks for a stream of events.
fn asks_events(fields: &Map<String, Value>) -> bool {
    fields.get("stream_format").and_then(Value::as_str) == Some("sse")
}

/// A client's request.
fn decode_request(_: &RequestHead, form: &Form) -> Result<Input, Fault> {
    let Fields { mut map, files } = form.fields()?;
    if let Some((name, _)) = files.first() {
        return Err(invalid(name, "is not a file this endpoint takes"));
    }
    let mut input = Input::new(take_string(&mut map, "input")?.ok_or_else(|| missing("input"))?);
    input.model = take_string(&mut map, "model")?;
    // A custom voice is an object, which stays native.
    if map.get("voice").is_some_and(Value::is_string) {
        input.voice = take_string(&mut map, "voice")?;
    }
    input.instructions = take_string(&mut map, "instructions")?;
    input.speed = take_number(&mut map, "speed")?;
    input.audio_format = take_string(&mut map, "response_format")?;
    map.remove("stream_format");
    input.native = Native::json::<Key>(map);
    Ok(input)
}

/// The call of `input`, in JSON. Audio of no format named is asked for as
/// `mp3`, the spec's default and the type a client is answered with, as a
/// compatible vendor may default to another.
fn post(
    head: &mut RequestHead,
    input: &Input,
    stream: bool,
) -> Result<Box<dyn Encoder<Input>>, Fault> {
    post_to(head, SPEECH, "application/json")?;
    let mut v = Map::new();
    set(&mut v, "model", input.model.clone());
    v.insert("input".into(), input.text.clone().into());
    set(&mut v, "voice", input.voice.clone());
    set(&mut v, "instructions", input.instructions.clone());
    let format = input.audio_format.as_deref().unwrap_or("mp3");
    v.insert("response_format".into(), format.into());
    set(&mut v, "speed", input.speed);
    merge_missing(&mut v, input.native.fields::<Key>());
    if stream {
        v.insert("stream_format".into(), "sse".into());
    }
    Ok(sends(json_bytes(&v)?))
}

/// The decoder of a provider's answer of `head`: a stream of events, or the
/// audio's bytes, a chunk per frame as it comes.
fn read_response(head: &http::response::Parts) -> Box<dyn Decoder<Output>> {
    if content::is_stream(head) {
        return ended_events(stream_event);
    }
    Box::new(|bytes: Bytes, out: &mut Vec<Output>| {
        out.push(Output::Audio(bytes));
        Ok(())
    })
}

/// The events of one stream event; `done` once it said its end.
fn stream_event(done: &mut bool, raw: &[u8], out: &mut Vec<Output>) -> Result<(), Fault> {
    let Some(v) = event_json(NAME, raw)? else {
        return Ok(());
    };
    match v["type"].as_str() {
        Some("speech.audio.delta") => {
            let audio = v["audio"].as_str().and_then(unb64);
            let audio = audio.ok_or_else(|| unreadable(NAME, "holds audio not in base64"))?;
            out.push(Output::Audio(audio));
        }
        Some("speech.audio.done") => {
            *done = true;
            if v["usage"].is_object() {
                let (usage, extras) = read_usage(&v["usage"], &[])?;
                out.push(Output::Usage(usage));
                out.extend(usage_extras::<Key>(extras).map(Output::Native));
            }
        }
        _ => out.push(Output::Native(Native::json::<Key>(v))),
    }
    Ok(())
}

/// The sink of a response to a client that `asked` so, in the framing it
/// asked for: a stream of events, or the audio's bytes, labelled as its
/// first bytes say ([`Labelled`]).
fn respond(reply: Reply, asked: &Map<String, Value>) -> EventSink<Output> {
    let mut head = reply.head();
    if asks_events(asked) {
        super::content_type(&mut head, "text/event-stream");
        return reply.write(head, Box::new(StreamEncoder::default()));
    }
    let format = asked.get("response_format").and_then(Value::as_str);
    let named = audio_type(format.unwrap_or("mp3"));
    EventSink::new(Labelled {
        unopened: Some((reply, head, named)),
        queued: VecDeque::new(),
        sink: None,
    })
}

/// The audio's bytes, and nothing else, which raw audio has no place for.
fn raw_audio(event: &Output, out: &mut Vec<Bytes>) -> Result<(), Fault> {
    if let Output::Audio(bytes) = event {
        out.push(bytes.clone());
    }
    Ok(())
}

/// The type of audio whose first bytes are `bytes`, of those a speech
/// endpoint answers, as a compatible vendor may answer another format than
/// asked. Raw PCM has no header, so audio asked as PCM is not read.
fn heard_type(named: &str, bytes: &[u8]) -> Option<&'static str> {
    let heard = super::sniff(bytes);
    let speech = |t: &&str| matches!(*t, "audio/wav" | "audio/mpeg" | "audio/flac" | "audio/aac");
    heard.filter(speech).filter(|_| named != "audio/pcm")
}

/// A response of raw audio whose head waits for its first audio, to be
/// labelled as [`heard_type`] says, else as the client `named` it. What
/// comes before is queued; a response with none is labelled as named.
struct Labelled {
    unopened: Option<(Reply, http::response::Parts, &'static str)>,
    queued: VecDeque<Result<Output, Fault>>,
    sink: Option<EventSink<Output>>,
}

impl Labelled {
    /// Writes the head, labelled by `audio`, the first audio if any.
    fn open(&mut self, audio: Option<&[u8]>) {
        if let Some((reply, mut head, named)) = self.unopened.take() {
            let label = audio.and_then(|a| heard_type(named, a)).unwrap_or(named);
            super::content_type(&mut head, label);
            self.sink = Some(reply.write(head, Box::new(raw_audio)));
        }
    }

    /// Sends what is queued, once the response is open.
    fn poll_send(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Fault>> {
        let Some(sink) = &mut self.sink else {
            return Poll::Ready(Ok(()));
        };
        while !self.queued.is_empty() {
            ready!(Pin::new(&mut *sink).poll_ready(cx))?;
            if let Some(item) = self.queued.pop_front() {
                Pin::new(&mut *sink).start_send(item)?;
            }
        }
        Poll::Ready(Ok(()))
    }
}

impl Sink<Result<Output, Fault>> for Labelled {
    type Error = Fault;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Fault>> {
        let this = self.get_mut();
        ready!(this.poll_send(cx))?;
        match &mut this.sink {
            Some(sink) => Pin::new(sink).poll_ready(cx),
            None => Poll::Ready(Ok(())),
        }
    }

    fn start_send(self: Pin<&mut Self>, item: Result<Output, Fault>) -> Result<(), Fault> {
        let this = self.get_mut();
        match &item {
            Ok(Output::Audio(bytes)) if !bytes.is_empty() => this.open(Some(bytes)),
            Ok(_) => {}
            Err(_) => this.open(None),
        }
        this.queued.push_back(item);
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Fault>> {
        let this = self.get_mut();
        ready!(this.poll_send(cx))?;
        match &mut this.sink {
            Some(sink) => Pin::new(sink).poll_flush(cx),
            None => Poll::Ready(Ok(())),
        }
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Fault>> {
        let this = self.get_mut();
        this.open(None);
        ready!(this.poll_send(cx))?;
        match &mut this.sink {
            Some(sink) => Pin::new(sink).poll_close(cx),
            None => Poll::Ready(Ok(())),
        }
    }
}

/// The content type of audio of file format `format`.
fn audio_type(format: &str) -> &'static str {
    match format {
        "mp3" => "audio/mpeg",
        "opus" => "audio/opus",
        "aac" => "audio/aac",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "pcm" => "audio/pcm",
        _ => "application/octet-stream",
    }
}

/// Writes a stream of events: each chunk at once, and the end, with the
/// usage reported, when the stream ends well.
#[derive(Debug, Default)]
struct StreamEncoder {
    usage: Option<Usage>,
    extras: Map<String, Value>,
}

impl StreamEncoder {
    fn encode_into(&mut self, event: &Output, out: &mut BytesMut) -> Result<(), Fault> {
        let v = match event {
            Output::Audio(bytes) => {
                let mut v = Map::new();
                v.insert("type".into(), "speech.audio.delta".into());
                v.insert("audio".into(), b64(bytes).into());
                Value::Object(v)
            }
            Output::Usage(u) => {
                *self.usage.get_or_insert_with(Usage::default) += u;
                return Ok(());
            }
            Output::Native(n) => match n.value::<Key>() {
                Some(v) => match as_usage_extras(v) {
                    Some(more) => {
                        merge_missing(&mut self.extras, more);
                        return Ok(());
                    }
                    None => v.clone(),
                },
                None => return Ok(()),
            },
            _ => return Ok(()),
        };
        out.extend_from_slice(sse_event(&v).as_bytes());
        Ok(())
    }

    fn end_into(&mut self, end: &Result<(), Fault>, out: &mut BytesMut) -> Result<(), Fault> {
        end.clone()?;
        let mut v = Map::new();
        v.insert("type".into(), "speech.audio.done".into());
        if let Some(usage) = self.usage {
            let mut u = write_usage(&usage);
            merge_missing(&mut u, &self.extras);
            v.insert("usage".into(), u.into());
        }
        out.extend_from_slice(sse_event(&v.into()).as_bytes());
        Ok(())
    }
}

impl Encoder<Output> for StreamEncoder {
    fn encode(&mut self, event: &Output, frames: &mut Vec<Bytes>) -> Result<(), Fault> {
        crate::edge::frame(frames, |out| self.encode_into(event, out))
    }

    fn end(&mut self, end: Result<(), Fault>, frames: &mut Vec<Bytes>) -> Result<(), Fault> {
        crate::edge::frame(frames, |out| self.end_into(&end, out))
    }
}
