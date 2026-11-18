//! DashScope's speech synthesis, `POST /services/audio/tts/SpeechSynthesizer`,
//! asked to stream: a format of the [`speech`](respan_gateway_media::speech)
//! kind, [`DashScopeSpeech`].

use super::{post_json, said_error, says_error};
use crate::edge::{bind_provider, ended_events, sends};
use respan_gateway_conversation::Usage;
use respan_gateway_conversation_http::fields::{base64_bytes, invalid, merge_missing, set};
use respan_gateway_core::{Connection, Fault, Format, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::HeaderValue;
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::{Connect, Encoder, ProviderIo, Upstream, native::Json};
use respan_gateway_media::speech::{Input, Output};
use serde_json::{Map, Value};

const NAME: &str = "dashscope-speech";

type Key = crate::Own<DashScopeSpeech>;
const SPEECH: &str = "/services/audio/tts/SpeechSynthesizer";
/// The audio formats DashScope writes.
const FORMATS: [&str; 4] = ["mp3", "wav", "pcm", "opus"];

/// DashScope's speech synthesis, as Model Studio serves its speech models
/// (`qwen-audio-3.1-tts-flash`): a `POST` of JSON to
/// `/services/audio/tts/SpeechSynthesizer`, asked to stream
/// (`X-DashScope-SSE: enable`), as its whole answer names the audio at a URL.
///
/// The text, voice, `instructions` (as `instruction`), speed (as `rate`) and
/// audio format are its `input`, with the input's native fields, such as
/// `sample_rate`. A voice is needed, and an audio format of `mp3` (unless
/// asked, as DashScope writes WAV for none), `wav`,
/// `pcm` or `opus`: another is refused, 400, before the call.
///
/// Each event's base64 `output.audio.data` decodes into an
/// [`Output::Audio`] chunk, in order; the event that ends it
/// (`finish_reason: "stop"`) holds the usage, its tokens as [`Output::Usage`],
/// and a stream cut before it fails `truncated`. An error event is the
/// fault it says, a 400 for a parameter refused.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DashScopeSpeech;

impl DashScopeSpeech {
    /// The format's name, `dashscope-speech`.
    pub const NAME: &'static str = NAME;
}

/// A bearer key, over HTTP, beside its base URL.
impl Upstream for DashScopeSpeech {
    fn name(&self) -> &str {
        NAME
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for DashScopeSpeech {
    fn name(&self) -> &str {
        NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        Ok(bind_provider(io, NAME, post, |_, _| {
            Some(ended_events(stream_event))
        }))
    }
}

/// The call of `input`, asked to stream.
fn post(head: &mut RequestHead, input: &Input) -> Result<Box<dyn Encoder<Input>>, Fault> {
    let voice = input.voice.clone();
    let voice = voice.ok_or_else(|| invalid("voice", "is needed by DashScope"))?;
    let format = input.audio_format.as_deref();
    if let Some(format) = format.filter(|f| !FORMATS.contains(f)) {
        return Err(invalid(
            "response_format",
            &format!("{format} is not written by DashScope: mp3, wav, pcm or opus"),
        ));
    }
    post_json(head, SPEECH)?;
    head.headers
        .insert("x-dashscope-sse", HeaderValue::from_static("enable"));
    let mut v = Map::new();
    v.insert("text".into(), input.text.clone().into());
    v.insert("voice".into(), voice.into());
    v.insert("format".into(), format.unwrap_or("mp3").into());
    set(&mut v, "instruction", input.instructions.clone());
    set(&mut v, "rate", input.speed);
    merge_missing(&mut v, input.native.fields::<Key>());
    let mut body = Map::new();
    set(&mut body, "model", input.model.clone());
    body.insert("input".into(), v.into());
    let body = serde_json::to_vec(&body)
        .map_err(|e| Fault::internal("a body could not be written").with_source(e))?;
    Ok(sends(Bytes::from(body)))
}

/// The events of one stream event; `done` once it said its end.
fn stream_event(done: &mut bool, raw: &[u8], out: &mut Vec<Output>) -> Result<(), Fault> {
    let Some(data) = respan_gateway_sse::event_data(raw) else {
        return Ok(());
    };
    let v: Value = serde_json::from_str(&data)
        .map_err(|_| Fault::upstream("DashScope's speech event is not JSON"))?;
    if says_error(&v) {
        return Err(said_error(Bytes::from(data), &v));
    }
    let audio = v["output"]["audio"]["data"].as_str().unwrap_or_default();
    if !audio.is_empty() {
        let audio = base64_bytes(audio)
            .ok_or_else(|| Fault::upstream("DashScope's speech event holds audio not in base64"))?;
        out.push(Output::Audio(Bytes::from(audio)));
    }
    // Each sentence's end repeats the usage so far; the last event's is all.
    if v["output"]["finish_reason"] == "stop" {
        *done = true;
        let count = |key: &str| v["usage"][key].as_u64().unwrap_or(0);
        let usage = Usage::new(count("input_tokens"), count("output_tokens"));
        if !usage.is_empty() {
            out.push(Output::Usage(usage));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_http::http;

    fn event(v: Value) -> Vec<u8> {
        format!("id:1\nevent:result\n:HTTP_STATUS/200\ndata:{v}\n\n").into_bytes()
    }

    #[test]
    fn a_stream_is_its_audio_in_order_and_the_last_events_usage() {
        let mut done = false;
        let mut out = Vec::new();
        let chunk = serde_json::json!({"output": {"type": "sentence-synthesis",
            "audio": {"data": "SUQz"}, "finish_reason": "null"}});
        let end = serde_json::json!({"output": {"type": "sentence-end", "audio": {"data": ""},
            "finish_reason": "null"}, "usage": {"input_tokens": 5, "output_tokens": 23}});
        let last = serde_json::json!({"output": {"audio": {"data": "", "url": "http://x"},
            "finish_reason": "stop"}, "usage": {"input_tokens": 5, "output_tokens": 23}});
        for v in [chunk, end, last] {
            stream_event(&mut done, &event(v), &mut out).expect("an event");
        }
        assert!(done);
        assert_eq!(
            out,
            [
                Output::Audio(Bytes::from_static(b"ID3")),
                Output::Usage(Usage::new(5, 23)),
            ]
        );
    }

    #[test]
    fn a_refused_parameter_in_the_stream_is_a_400() {
        let v = serde_json::json!({"code": "InvalidParameter", "message": "bad voice"});
        let fault = stream_event(&mut false, &event(v), &mut Vec::new()).expect_err("a fault");
        let status = fault
            .get::<respan_gateway_http_faults::Status>()
            .map(|s| s.0);
        assert_eq!(status, Some(http::StatusCode::BAD_REQUEST));
        assert_eq!(fault.message(), "bad voice");
    }
}
