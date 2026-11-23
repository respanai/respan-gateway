//! Gemini's transcription models through `generateContent`: a format of the
//! [`transcription`](respan_gateway_media::transcription) kind,
//! [`GeminiTranscriptions`].

use super::is_segment;
use crate::edge::{bind_provider, sends};
use respan_gateway_conversation_http::fields::invalid;
use respan_gateway_conversation_http::gemini::{media_part, usage_of};
use respan_gateway_core::{Connection, Fault, Format, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::HeaderValue;
use respan_gateway_http::http::header::CONTENT_TYPE;
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::{Connect, Encoder, KeyIn, ProviderIo, Upstream, Whole, native::Json};
use respan_gateway_media::transcription::{Input, Output, Segment, Task, Transcript, Word};
use serde_json::{Map, Value, json};

const NAME: &str = "gemini-transcriptions";

type Key = crate::Own<GeminiTranscriptions>;

/// Gemini's transcription models (`gemini-3.5-transcribe`) as the Gemini API
/// serves them: a `POST` to `.../models/{model}:generateContent`, its
/// answer's `audioTranscription` parts the transcript.
///
/// A call is one user turn: the prompt, if any, as text, then the audio, as
/// `inlineData` (or `fileData`, for audio named by URL). The language is the
/// `languageCodes` of its `generationConfig`'s `audioTranscriptionConfig`,
/// the temperature the config's. A translation is refused, 400, before the
/// call. Its native fields go in the body, such as an
/// `audioTranscriptionConfig` of `wordTimestamp` or `diarization`.
///
/// An answer's text is its parts' `audioTranscription` texts, its words
/// theirs, timed, the last one's end its length, and each part a speaker is
/// named for a segment. Its usage is `usageMetadata`'s tokens.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GeminiTranscriptions;

impl GeminiTranscriptions {
    /// The format's name, `gemini-transcriptions`.
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
        Ok(bind_provider(io, NAME, call, |_, _| {
            Some(Box::new(Whole::new(read_response)))
        }))
    }
}

/// The call of `input`: one user turn of its prompt and audio.
fn call(head: &mut RequestHead, input: &Input) -> Result<Box<dyn Encoder<Input>>, Fault> {
    if input.task == Task::Translate {
        return Err(Fault::bad_request("Gemini does not translate audio"));
    }
    let model = input.model.as_deref().unwrap_or_default();
    let model = model.strip_prefix("models/").unwrap_or(model);
    if !is_segment(model) {
        return Err(Fault::bad_request(
            "a Gemini transcription needs a model, such as gemini-3.5-transcribe",
        ));
    }
    head.uri = format!("/models/{model}:generateContent")
        .parse()
        .map_err(|e| Fault::internal("a call's path is no path").with_source(e))?;
    let json = HeaderValue::from_static("application/json");
    head.headers.insert(CONTENT_TYPE, json);
    let body = serde_json::to_vec(&body_of(input)?)
        .map_err(|e| Fault::internal("a body could not be written").with_source(e))?;
    Ok(sends(Bytes::from(body)))
}

/// The body of `input`'s call.
fn body_of(input: &Input) -> Result<Map<String, Value>, Fault> {
    let mut native = input.native.fields::<Key>().clone();
    let mut parts = Vec::new();
    parts.extend(input.prompt.as_ref().map(|text| json!({ "text": text })));
    parts.push(media_part(&input.audio));
    let mut config = match native.remove("generationConfig") {
        Some(Value::Object(config)) => config,
        None => Map::new(),
        Some(_) => return Err(invalid("generationConfig", "must be an object")),
    };
    if let Some(temperature) = input.temperature {
        config.insert("temperature".into(), temperature.into());
    }
    if let Some(language) = &input.language {
        let audio = config
            .entry("audioTranscriptionConfig")
            .or_insert_with(|| json!({}));
        match audio.as_object_mut() {
            Some(audio) => audio.insert("languageCodes".into(), json!([language])),
            None => {
                let field = "generationConfig.audioTranscriptionConfig";
                return Err(invalid(field, "must be an object"));
            }
        };
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

/// The events of an answer: its transcript, empty of audio with no speech,
/// then its usage. One that says why it has no transcript is a fault.
fn read_response(raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
    let v = serde_json::from_slice::<Value>(&raw)
        .map_err(|_| Fault::upstream(format!("the provider's {NAME} response is not JSON")))?;
    let mut transcript = Transcript::default();
    let (mut texts, mut why) = (Vec::new(), None);
    for candidate in v["candidates"].as_array().into_iter().flatten() {
        let parts = candidate["content"]["parts"].as_array();
        for part in parts.into_iter().flatten().filter(|p| p["thought"] != true) {
            let heard = &part["audioTranscription"];
            let Some(text) = heard["text"].as_str().or(part["text"].as_str()) else {
                continue;
            };
            if text.trim().is_empty() {
                continue;
            }
            let from = transcript.words.len();
            let words = heard["words"].as_array().into_iter().flatten();
            transcript.words.extend(words.filter_map(word_of));
            if let Some(speaker) = heard["speakerLabel"].as_str() {
                let said = transcript.words.get(from..).unwrap_or_default();
                let start = said.first().map_or(0.0, |w| w.start);
                let end = said.last().map_or(0.0, |w| w.end);
                let mut segment = Segment::new(start, end, text.trim());
                segment.speaker = Some(speaker.to_owned());
                transcript.segments.push(segment);
            }
            texts.push(text.trim());
        }
        why = why.or(candidate["finishReason"].as_str().filter(|r| *r != "STOP"));
    }
    let why = v["promptFeedback"]["blockReason"].as_str().or(why);
    if let (true, Some(why)) = (texts.is_empty(), why) {
        return Err(Fault::upstream(format!(
            "Gemini made no transcript ({why})"
        )));
    }
    transcript.text = texts.join(" ");
    transcript.duration = transcript.words.last().map(|w| w.end);
    out.push(Output::Transcript(transcript));
    let usage = &v["usageMetadata"];
    if usage.is_object() {
        out.push(Output::Usage(usage_of(usage)?));
    }
    Ok(())
}

/// A word of a transcript, timed by its offsets, such as `"0.300s"`.
fn word_of(word: &Value) -> Option<Word> {
    let at = |key: &str| word[key].as_str()?.strip_suffix('s')?.parse::<f64>().ok();
    let text = word["word"].as_str()?;
    Some(Word::new(text, at("startOffset")?, at("endOffset")?))
}
