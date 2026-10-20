//! OpenAI Audio Speech: `POST /audio/speech` (JSON), answered with the
//! audio's bytes or, if asked (`stream_format: "sse"`), a stream of events.

use super::{
    Fields, Form, as_usage_extras, b64, bind_client, event_json, foreign, invalid, json_bytes,
    kept, merge_missing, missing, native_of, post_to, read_usage, sends, set, sse_event, streams,
    take_number, take_string, unb64, unreadable, usage_extras, write_usage,
};
use crate::dialect::{self, Spec};
use crate::edge::ended_events;
use respan_gateway_conversation::Usage;
use respan_gateway_core::Provider;
use respan_gateway_core::{Connection, Fault, Format, NativeValue};
use respan_gateway_http::bytes::{Bytes, BytesMut};
use respan_gateway_http::content;
use respan_gateway_http::http::{self, Method, request::Parts as RequestHead};
use respan_gateway_http::path::path_ends_with;
use respan_gateway_http::{ClientIo, Decoder, Encoder, ProviderIo};
use respan_gateway_http::{Connect, Upstream};
use respan_gateway_media::speech::{Input, Output};
use serde_json::{Map, Value};

const NAME: &str = "openai-speech";
const SPEECH: &str = "/audio/speech";

/// OpenAI Audio Speech, as OpenAI or a compatible vendor serves it: a
/// `POST` to a path ending in `/audio/speech`.
///
/// The audio's bytes decode into [`Output::Audio`] chunks as they come, and
/// are relayed so, the provider read no faster than the client takes them.
/// A stream of events decodes into the same chunks, from each
/// `speech.audio.delta`, and the usage of its `speech.audio.done`, failing
/// `truncated` without one. A client is answered in the framing it asked
/// for: the audio's bytes, of the type its `response_format` names, or a
/// stream of events.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenAiSpeech;

impl OpenAiSpeech {
    /// The format's name, `openai-speech`, which tags its native fields.
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
        Ok(bind_client(io, decode_request, response_encoder))
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
        Ok(dialect::bind(*self, io))
    }
}

/// Its spec's parts, which a vendor's dialect runs around.
impl Spec for OpenAiSpeech {
    type Input = Input;
    type Output = Output;
    const NAME: &'static str = NAME;

    fn streams(&self, io: &ProviderIo) -> bool {
        streams(io, &[SPEECH], asks_events)
    }

    fn post(
        &self,
        head: &mut RequestHead,
        input: &Input,
        stream: bool,
    ) -> Result<Box<dyn Encoder<Input>>, Fault> {
        post(head, input, stream)
    }

    fn read_response(
        &self,
        head: &http::response::Parts,
        _: &http::Request<Bytes>,
    ) -> Option<Box<dyn Decoder<Output>>> {
        Some(read_response(head))
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
    input.native = native_of(NAME, map);
    Ok(input)
}

/// The call of `input`, in JSON.
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
    set(&mut v, "response_format", input.audio_format.clone());
    set(&mut v, "speed", input.speed);
    merge_missing(&mut v, &kept(NAME, &input.native)?);
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
                out.extend(usage_extras(NAME, extras).map(Output::Native));
            }
        }
        _ => out.push(Output::Native(NativeValue::new(NAME, v))),
    }
    Ok(())
}

/// The encoder of a response to a client's `request`, in the framing it
/// asked for.
fn response_encoder(
    _request: &http::Request<Bytes>,
    asked: &Map<String, Value>,
    head: &mut http::response::Parts,
) -> Box<dyn Encoder<Output>> {
    if asks_events(asked) {
        super::content_type(head, "text/event-stream");
        return Box::new(StreamEncoder::default());
    }
    let format = asked.get("response_format").and_then(Value::as_str);
    super::content_type(head, audio_type(format.unwrap_or("mp3")));
    Box::new(|event: &Output, out: &mut Vec<Bytes>| {
        match event {
            Output::Audio(bytes) => out.push(bytes.clone()),
            Output::Native(n) if &*n.format != NAME => return Err(foreign(NAME, n)),
            // Raw audio has no place for anything else.
            _ => {}
        }
        Ok(())
    })
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
            Output::Native(n) if &*n.format != NAME => return Err(foreign(NAME, n)),
            Output::Native(n) => match as_usage_extras(&n.value) {
                Some(more) => {
                    merge_missing(&mut self.extras, more);
                    return Ok(());
                }
                None => (*n.value).clone(),
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
