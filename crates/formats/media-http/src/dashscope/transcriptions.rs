//! DashScope's speech recognition through `multimodal-generation`: a format
//! of the [`transcription`](respan_gateway_media::transcription) kind,
//! [`DashScopeTranscriptions`].

use super::{post_json, said_error, says_error};
use crate::edge::{bind_provider, sends};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use respan_gateway_conversation::{MediaSource, Usage};
use respan_gateway_conversation_http::fields::{invalid, merge_missing, set};
use respan_gateway_core::{Connection, Fault, Format, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::{Connect, Decoder, Encoder, ProviderIo, Upstream, Whole, native::Json};
use respan_gateway_media::transcription::{Input, Output, Task, Transcript};
use serde_json::{Map, Value, json};

const NAME: &str = "dashscope-transcriptions";

type Key = crate::Own<DashScopeTranscriptions>;
const GENERATION: &str = "/services/aigc/multimodal-generation/generation";

/// Audio MIME types and the format DashScope names each by.
const FORMATS: &[(&str, &str)] = &[
    ("audio/mpeg", "mp3"),
    ("audio/mp3", "mp3"),
    ("audio/wav", "wav"),
    ("audio/x-wav", "wav"),
    ("audio/mp4", "m4a"),
    ("audio/x-m4a", "m4a"),
    ("audio/ogg", "ogg"),
    ("audio/opus", "opus"),
    ("audio/flac", "flac"),
    ("audio/webm", "webm"),
    ("audio/aac", "aac"),
    ("audio/amr", "amr"),
];

/// DashScope's speech recognition, as Model Studio serves its recognition
/// models of a whole file (`qwen-audio-3.1-asr-flash`, `qwen3-asr-flash`,
/// `fun-asr-flash-*`): a `POST` of JSON to
/// `/services/aigc/multimodal-generation/generation`, a user message of the
/// audio, as a data URI of its bytes or its URL, for DashScope to fetch.
///
/// The audio's `format`, from its type, the language (as `language_hints`)
/// and the input's native fields, such as `enable_itn`, are its
/// `parameters`. A translation, a prompt and a temperature are refused, 400,
/// before the call.
///
/// An answer's text is its `output.text`, or its first choice's text, a
/// [`Transcript`] of the seconds its usage counts. Its usage is the seconds
/// billed, [`Output::Seconds`], and its tokens, if any, [`Output::Usage`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DashScopeTranscriptions;

impl DashScopeTranscriptions {
    /// The format's name, `dashscope-transcriptions`.
    pub const NAME: &'static str = NAME;
}

/// A bearer key, over HTTP, beside its base URL.
impl Upstream for DashScopeTranscriptions {
    fn name(&self) -> &str {
        NAME
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for DashScopeTranscriptions {
    fn name(&self) -> &str {
        NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        Ok(bind_provider(io, NAME, post, |_, _| Some(read_response())))
    }
}

/// The call of `input`: a user message of its audio.
fn post(head: &mut RequestHead, input: &Input) -> Result<Box<dyn Encoder<Input>>, Fault> {
    if input.task == Task::Translate {
        return Err(Fault::bad_request("DashScope does not translate audio"));
    }
    if input.prompt.is_some() {
        return Err(invalid("prompt", "is not taken by DashScope"));
    }
    if input.temperature.is_some() {
        return Err(invalid("temperature", "is not taken by DashScope"));
    }
    let audio = &input.audio;
    let mime = audio.mime_type.split(';').next().unwrap_or_default().trim();
    let mime = mime.to_ascii_lowercase();
    let data = match &audio.source {
        MediaSource::Bytes(bytes) => format!("data:{mime};base64,{}", STANDARD.encode(bytes)),
        MediaSource::Url(url) => url.clone(),
    };
    let named = audio.name.as_deref().and_then(|n| n.rsplit_once('.'));
    let format = FORMATS
        .iter()
        .find(|(m, _)| *m == mime)
        .map(|(_, f)| (*f).to_owned())
        .or_else(|| named.map(|(_, ext)| ext.to_ascii_lowercase()))
        .ok_or_else(|| invalid("file", "is audio of a type DashScope does not name"))?;
    post_json(head, GENERATION)?;
    let mut parameters = Map::new();
    parameters.insert("format".into(), format.into());
    let hints = input.language.clone().map(|l| vec![Value::from(l)]);
    set(&mut parameters, "language_hints", hints);
    merge_missing(&mut parameters, input.native.fields::<Key>());
    let mut body = Map::new();
    set(&mut body, "model", input.model.clone());
    let message = json!({ "role": "user", "content": [{ "audio": data }] });
    body.insert("input".into(), json!({ "messages": [message] }));
    body.insert("parameters".into(), parameters.into());
    let body = serde_json::to_vec(&body)
        .map_err(|e| Fault::internal("a body could not be written").with_source(e))?;
    Ok(sends(Bytes::from(body)))
}

/// The decoder of DashScope's whole answer.
fn read_response() -> Box<dyn Decoder<Output>> {
    Box::new(Whole::new(|raw: Bytes, out: &mut Vec<Output>| {
        let v: Value = serde_json::from_slice(&raw).unwrap_or_default();
        if says_error(&v) {
            return Err(said_error(raw, &v));
        }
        let output = &v["output"];
        let chosen = &output["choices"][0]["message"]["content"][0]["text"];
        let text = output["text"].as_str().or(chosen.as_str());
        let text = text.ok_or_else(|| Fault::upstream("DashScope's answer holds no transcript"))?;
        let usage = &v["usage"];
        let seconds = usage["duration"].as_f64().or(usage["seconds"].as_f64());
        let mut transcript = Transcript::new(text.trim());
        transcript.duration = seconds;
        out.push(Output::Transcript(transcript));
        let count = |key: &str| usage[key].as_u64().unwrap_or(0);
        let tokens = Usage::new(count("input_tokens"), count("output_tokens"));
        if !tokens.is_empty() {
            out.push(Output::Usage(tokens));
        }
        out.extend(seconds.map(Output::Seconds));
        Ok(())
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(v: Value) -> Vec<Output> {
        let mut out = Vec::new();
        let mut decoder = read_response();
        decoder
            .decode(Bytes::from(v.to_string()), &mut out)
            .expect("a body");
        decoder.end(Ok(()), &mut out).expect("an answer");
        out
    }

    #[test]
    fn an_answer_is_its_text_its_seconds_and_its_tokens() {
        let qwen_audio = json!({"output": {"output": {"text": "Hello. "}, "text": "Hello. "},
            "usage": {"duration": 3, "input_tokens": 32, "output_tokens": 5}});
        let qwen3 = json!({"output": {"choices": [{"message": {"content": [{"text": "Hello."}]}}]},
            "usage": {"seconds": 3, "input_tokens": 32, "output_tokens": 5}});
        for v in [qwen_audio, qwen3] {
            let mut transcript = Transcript::new("Hello.");
            transcript.duration = Some(3.0);
            assert_eq!(
                read(v),
                [
                    Output::Transcript(transcript),
                    Output::Usage(Usage::new(32, 5)),
                    Output::Seconds(3.0),
                ]
            );
        }
    }
}
