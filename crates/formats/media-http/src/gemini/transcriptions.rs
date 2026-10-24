//! Gemini's models as transcribers, through `generateContent`: a format of
//! the [`transcription`](respan_gateway_media::transcription) kind,
//! [`GeminiTranscriptions`].

use super::is_segment;
use crate::dialect::{self, Spec};
use crate::edge::sends;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use respan_gateway_conversation::MediaSource;
use respan_gateway_conversation_http::fields::{invalid, kept};
use respan_gateway_conversation_http::gemini::usage_of;
use respan_gateway_core::{Connection, ErrorCode, Fault, Format, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::header::CONTENT_TYPE;
use respan_gateway_http::http::{
    self, HeaderValue, request::Parts as RequestHead, response::Parts as ResponseHead,
};
use respan_gateway_http::{Connect, Decoder, Encoder, KeyIn, ProviderIo, Upstream, Whole};
use respan_gateway_media::transcription::{Input, Output, Task, Transcript};
use serde_json::{Map, Value, json};

const NAME: &str = "gemini-transcriptions";

/// The most a request holds, audio inline included, as Gemini documents it.
const INLINE_LIMIT: usize = 20 * 1024 * 1024;

/// Gemini's models as transcribers, as the Gemini API serves them: a `POST`
/// to `.../models/{model}:generateContent` of one user turn, the audio as
/// `inlineData` (or `fileData`, for audio named by URL) and an instruction
/// as text, the transcript the answer's text.
///
/// The instruction asks for the words spoken, or, for a translation, their
/// English; the `language` and `prompt` go into it, and the `temperature`
/// into `generationConfig`. Audio over 20 MB, the most a request
/// holds, is refused, 400, before any call. The native fields of this format
/// go in the body,
/// `generationConfig` merged with what it says. An answer's text, but not
/// its thoughts', is the transcript, as is a transcribing model's
/// `audioTranscription` part (`gemini-3.5-transcribe`'s); its usage is
/// `usageMetadata`'s tokens. An answer with no text, saying why, is
/// refused, 400 `no_transcript`.
///
/// It serves providers only: no client speaks it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GeminiTranscriptions;

impl GeminiTranscriptions {
    /// The format's name, `gemini-transcriptions`, which tags its native
    /// fields.
    pub const NAME: &'static str = NAME;
}

/// Its spec's: the key in `x-goog-api-key`, over HTTP.
impl Upstream for GeminiTranscriptions {
    fn name(&self) -> &str {
        NAME
    }

    fn key(&self) -> KeyIn {
        KeyIn::Header("x-goog-api-key")
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for GeminiTranscriptions {
    fn name(&self) -> &str {
        NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        Ok(dialect::bind(*self, io))
    }
}

/// Its spec's parts, which a vendor's dialect runs around.
impl Spec for GeminiTranscriptions {
    type Input = Input;
    type Output = Output;
    const NAME: &'static str = NAME;

    fn post(
        &self,
        head: &mut RequestHead,
        input: &Input,
        _: bool,
    ) -> Result<Box<dyn Encoder<Input>>, Fault> {
        call(head, input)
    }

    fn read_response(
        &self,
        _: &ResponseHead,
        _: &http::Request<Bytes>,
    ) -> Option<Box<dyn Decoder<Output>>> {
        Some(Box::new(Whole::new(read_response)))
    }
}

/// The call of `input`: one user turn of its audio and instruction.
fn call(head: &mut RequestHead, input: &Input) -> Result<Box<dyn Encoder<Input>>, Fault> {
    let model = input.model.as_deref().unwrap_or_default();
    let model = model.strip_prefix("models/").unwrap_or(model);
    if !is_segment(model) {
        return Err(Fault::bad_request(
            "a Gemini transcription needs a model, such as gemini-2.5-flash",
        ));
    }
    let body = serde_json::to_vec(&body_of(input)?)
        .map_err(|e| Fault::internal("a body could not be written").with_source(e))?;
    head.uri = format!("/models/{model}:generateContent")
        .parse()
        .map_err(|e| Fault::internal("a call's path is no path").with_source(e))?;
    let json = HeaderValue::from_static("application/json");
    head.headers.insert(CONTENT_TYPE, json);
    Ok(sends(Bytes::from(body)))
}

/// A call's body: its turn, its `generationConfig`, and its native fields.
fn body_of(input: &Input) -> Result<Map<String, Value>, Fault> {
    let mut native = kept(NAME, &input.native)?;
    let audio = match &input.audio.source {
        MediaSource::Bytes(bytes) if bytes.len().div_ceil(3) * 4 > INLINE_LIMIT => {
            return Err(invalid("file", "is over the 20 MB Gemini takes inline"));
        }
        MediaSource::Bytes(bytes) => json!({
            "inlineData": { "mimeType": input.audio.mime_type, "data": STANDARD.encode(bytes) },
        }),
        MediaSource::Url(url) => json!({
            "fileData": { "mimeType": input.audio.mime_type, "fileUri": url },
        }),
    };
    let parts = json!([audio, { "text": instruction_of(input) }]);
    let mut config = match native.remove("generationConfig") {
        Some(Value::Object(config)) => config,
        None => Map::new(),
        Some(_) => return Err(invalid("generationConfig", "must be an object")),
    };
    if let Some(temperature) = input.temperature {
        config.insert("temperature".into(), temperature.into());
    }
    let mut v = Map::new();
    v.insert(
        "contents".into(),
        json!([{ "role": "user", "parts": parts }]),
    );
    if !config.is_empty() {
        v.insert("generationConfig".into(), config.into());
    }
    for (key, value) in native {
        v.entry(key).or_insert(value);
    }
    Ok(v)
}

/// What the model is asked to do with the audio.
fn instruction_of(input: &Input) -> String {
    let mut text = match input.task {
        Task::Translate => String::from("Translate the speech in this audio to English."),
        _ => String::from("Transcribe the speech in this audio verbatim."),
    };
    if let Some(language) = &input.language {
        text.push_str(&format!(" The language spoken is {language}."));
    }
    if let Some(prompt) = &input.prompt {
        text.push_str(&format!(" {prompt}"));
    }
    text.push_str(" Answer with the text alone.");
    text
}

/// The events of an answer: its transcript, then its usage. One with no
/// text and a reason is refused, saying why.
fn read_response(raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
    let v = serde_json::from_slice::<Value>(&raw)
        .map_err(|_| Fault::upstream(format!("the provider's {NAME} response is not JSON")))?;
    let (mut said, mut why) = (Vec::new(), None);
    for candidate in v["candidates"].as_array().into_iter().take(1).flatten() {
        let parts = candidate["content"]["parts"].as_array();
        for part in parts.into_iter().flatten().filter(|p| p["thought"] != true) {
            said.extend(part["text"].as_str());
            said.extend(part["audioTranscription"]["text"].as_str());
        }
        why = candidate["finishReason"].as_str().filter(|r| *r != "STOP");
    }
    let why = v["promptFeedback"]["blockReason"].as_str().or(why);
    if said.is_empty()
        && let Some(why) = why
    {
        let message = format!("Gemini made no transcript ({why})");
        return Err(Fault::bad_request(message).with(ErrorCode("no_transcript".into())));
    }
    out.push(Output::Transcript(Transcript::new(said.join(""))));
    let usage = &v["usageMetadata"];
    if usage.is_object() {
        out.push(Output::Usage(usage_of(usage)?));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_conversation::Media;
    use respan_gateway_conversation_http::fields::native_of;
    use respan_gateway_core::NativeFields;

    fn input() -> Input {
        let mut input = Input::new(Media::new(
            "audio/wav",
            MediaSource::Bytes(vec![1, 2].into()),
        ));
        input.model = Some("gemini-2.5-flash".into());
        input
    }

    fn sent(input: &Input) -> Result<(http::request::Parts, Value), Fault> {
        let (mut head, ()) = http::Request::new(()).into_parts();
        let mut encoder = call(&mut head, input)?;
        let mut out = Vec::new();
        encoder.encode(input, &mut out)?;
        let body = serde_json::from_slice(&out.concat()).unwrap_or_default();
        Ok((head, body))
    }

    fn native(format: &str, v: Value) -> Option<NativeFields> {
        native_of(format, v.as_object().cloned().unwrap_or_default())
    }

    #[test]
    fn a_call_is_one_turn_of_the_audio_then_the_instruction() {
        let mut i = input();
        i.language = Some("fr".into());
        i.prompt = Some("Names: Zoé.".into());
        i.temperature = Some(0.2);
        let (head, body) = sent(&i).expect("sent");
        assert_eq!(
            head.uri.to_string(),
            "/models/gemini-2.5-flash:generateContent"
        );
        let turn = &body["contents"][0];
        assert_eq!(turn["role"], "user");
        assert_eq!(turn["parts"][0]["inlineData"]["mimeType"], "audio/wav");
        assert_eq!(turn["parts"][0]["inlineData"]["data"], "AQI=");
        let text = turn["parts"][1]["text"].as_str().unwrap_or_default();
        assert!(text.starts_with("Transcribe"));
        assert!(text.contains("The language spoken is fr.") && text.contains("Names: Zoé."));
        assert_eq!(body["generationConfig"]["temperature"], 0.2);
    }

    #[test]
    fn audio_at_a_url_is_a_file_and_a_translation_asks_for_english() {
        let mut i = input();
        i.audio = Media::new("audio/mp3", MediaSource::Url("gs://b/a.mp3".into()));
        i.task = Task::Translate;
        let (_, body) = sent(&i).expect("sent");
        assert_eq!(
            body["contents"][0]["parts"][0]["fileData"]["fileUri"],
            "gs://b/a.mp3"
        );
        let text = body["contents"][0]["parts"][1]["text"]
            .as_str()
            .unwrap_or_default();
        assert!(text.starts_with("Translate"));
    }

    #[test]
    fn a_call_needs_a_model() {
        let mut i = input();
        i.model = None;
        assert!(sent(&i).is_err());
        i.model = Some("../x".into());
        assert!(sent(&i).is_err());
    }

    #[test]
    fn its_native_fields_are_the_body_and_generation_config_merges() {
        let mut i = input();
        i.temperature = Some(0.5);
        i.native = native(
            NAME,
            json!({"generationConfig": {"maxOutputTokens": 9}, "safetySettings": []}),
        );
        let (_, body) = sent(&i).expect("sent");
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 9);
        assert_eq!(body["generationConfig"]["temperature"], 0.5);
        assert!(body["safetySettings"].is_array());
    }

    #[test]
    fn another_formats_fields_and_oversized_audio_are_refused() {
        let mut i = input();
        i.native = native("openai-transcriptions", json!({"response_format": "json"}));
        assert!(sent(&i).is_err());
        let mut i = input();
        i.audio = Media::new(
            "audio/wav",
            MediaSource::Bytes(vec![0; 16 * 1024 * 1024].into()),
        );
        assert!(sent(&i).is_err());
    }

    #[test]
    fn an_answer_is_its_text_then_its_usage() {
        let raw = json!({
            "candidates": [{"content": {"parts": [
                {"text": "thinking", "thought": true},
                {"text": "Hello "}, {"text": "world."},
            ]}, "finishReason": "STOP"}],
            "usageMetadata": {
                "promptTokenCount": 40, "candidatesTokenCount": 3, "totalTokenCount": 43,
                "promptTokensDetails": [{"modality": "AUDIO", "tokenCount": 32}],
            },
        })
        .to_string();
        let mut out = Vec::new();
        read_response(Bytes::from(raw), &mut out).expect("read");
        assert_eq!(out[0], Output::Transcript(Transcript::new("Hello world.")));
        let Output::Usage(usage) = &out[1] else {
            panic!("no usage: {out:?}");
        };
        assert_eq!((usage.input_tokens, usage.output_tokens), (40, 3));
        assert_eq!(out.len(), 2);
        // A transcribing model's own part.
        let raw = json!({"candidates": [{"content": {"parts": [
            {"audioTranscription": {"text": "Hello from the gateway."}},
        ]}, "finishReason": "STOP"}]});
        let mut out = Vec::new();
        read_response(Bytes::from(raw.to_string()), &mut out).expect("read");
        assert_eq!(
            out,
            [Output::Transcript(Transcript::new(
                "Hello from the gateway."
            ))]
        );
    }

    #[test]
    fn silence_is_an_empty_transcript_and_a_block_is_refused() {
        let silent = json!({"candidates": [{"content": {"parts": []}, "finishReason": "STOP"}]});
        let mut out = Vec::new();
        read_response(Bytes::from(silent.to_string()), &mut out).expect("read");
        assert_eq!(out, [Output::Transcript(Transcript::new(""))]);
        for blocked in [
            json!({"promptFeedback": {"blockReason": "SAFETY"}}),
            json!({"candidates": [{"finishReason": "PROHIBITED_CONTENT"}]}),
        ] {
            let refused = read_response(Bytes::from(blocked.to_string()), &mut Vec::new());
            assert!(refused.is_err(), "{blocked}");
        }
        assert!(read_response(Bytes::from("nope"), &mut Vec::new()).is_err());
    }
}
