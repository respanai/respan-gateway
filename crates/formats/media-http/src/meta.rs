//! Meta's speech recognition, over HTTP: a provider format of the
//! [`transcription`](respan_gateway_media::transcription) kind,
//! [`MetaTranscriptions`].

use crate::edge::{bind_provider, sends};
use crate::openai::multipart::{self, Writer};
use respan_gateway_conversation::MediaSource;
use respan_gateway_conversation_http::fields::{invalid, merge_missing, set};
use respan_gateway_core::{Connection, Fault, Format, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::HeaderValue;
use respan_gateway_http::http::header::CONTENT_TYPE;
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::{Connect, Encoder, ProviderIo, Upstream, Whole, native::Json};
use respan_gateway_media::transcription::{Input, Output, Segment, Task, Transcript};
use serde_json::{Map, Value};

const NAME: &str = "meta-transcriptions";

type Key = crate::Own<MetaTranscriptions>;

/// Meta's speech recognition (`muse-voice-transcribe-1.0`): a `POST` to
/// `/asr/transcribe` of a form, at the API's version, its base URL
/// `https://api.meta.ai/v1`.
///
/// The form is a `request`, JSON of the model, the `audioEncoding`, `WAV`,
/// and the input's native fields, such as `mode`, `keywords` or
/// `languageBias`, and the `audio`, its bytes. Meta takes only a WAV of mono
/// 16-bit PCM at 16 or 24 kHz and refuses another itself. Audio at a URL, a
/// translation, a prompt and a temperature are refused, 400, before the
/// call; a language is not sent, as Meta takes languages by name, as a bias.
///
/// An answer's text is its `transcript`, its length its `audioDurationMs`,
/// which is also the seconds billed, [`Output::Seconds`], and its `turns`,
/// which a `mode` asks for, the segments, each with its speaker, if named.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MetaTranscriptions;

impl MetaTranscriptions {
    /// The format's name, `meta-transcriptions`.
    pub const NAME: &'static str = NAME;
}

/// A bearer key, over HTTP, beside its base URL.
impl Upstream for MetaTranscriptions {
    fn name(&self) -> &str {
        NAME
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for MetaTranscriptions {
    fn name(&self) -> &str {
        NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        Ok(bind_provider(io, NAME, post, |_, _| {
            Some(Box::new(Whole::new(read_response)))
        }))
    }
}

/// The call of `input`: a form of its settings and its audio.
fn post(head: &mut RequestHead, input: &Input) -> Result<Box<dyn Encoder<Input>>, Fault> {
    if input.task == Task::Translate {
        return Err(Fault::bad_request("Meta does not translate audio"));
    }
    if input.prompt.is_some() {
        return Err(invalid("prompt", "is not taken by Meta"));
    }
    if input.temperature.is_some() {
        return Err(invalid("temperature", "is not taken by Meta"));
    }
    let MediaSource::Bytes(audio) = &input.audio.source else {
        return Err(Fault::bad_request(
            "audio at a URL cannot be sent as a file",
        ));
    };
    let mut request = Map::new();
    set(&mut request, "model", input.model.clone());
    request.insert("audioEncoding".into(), "WAV".into());
    merge_missing(&mut request, input.native.fields::<Key>());
    let mut form = Writer::with_boundary(multipart::random_boundary());
    respan_gateway_http::path::set_path(head, "/asr/transcribe")?;
    let content_type = HeaderValue::from_str(&form.content_type())
        .map_err(|e| Fault::internal("a content type is no header").with_source(e))?;
    head.headers.insert(CONTENT_TYPE, content_type);
    form.text("request", &Value::Object(request).to_string());
    form.file("audio", "audio.wav", "audio/wav", audio);
    Ok(sends(form.finish()))
}

/// The events of an answer: its transcript, then the seconds billed.
fn read_response(raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
    let v = serde_json::from_slice::<Value>(&raw)
        .map_err(|_| Fault::upstream(format!("the provider's {NAME} response is not JSON")))?;
    let text = v["transcript"].as_str();
    let text = text.ok_or_else(|| Fault::upstream("Meta's answer holds no transcript"))?;
    let seconds = |ms: &Value| ms.as_f64().map(|ms| ms / 1000.0);
    let mut transcript = Transcript::new(text);
    transcript.duration = seconds(&v["audioDurationMs"]);
    for turn in v["turns"].as_array().into_iter().flatten() {
        let start = seconds(&turn["startMs"]).unwrap_or(0.0);
        let end = seconds(&turn["endMs"]).unwrap_or(start);
        let text = turn["transcript"].as_str().unwrap_or_default();
        let mut segment = Segment::new(start, end, text);
        segment.speaker = turn["speaker"].as_str().map(str::to_owned);
        transcript.segments.push(segment);
    }
    let billed = transcript.duration;
    out.push(Output::Transcript(transcript));
    out.extend(billed.map(Output::Seconds));
    Ok(())
}
