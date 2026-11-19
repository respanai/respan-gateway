//! Gemini's speech models through `interactions`: a format of the
//! [`speech`](respan_gateway_media::speech) kind, [`GeminiSpeech`].

use crate::edge::{bind_provider, sends};
use respan_gateway_conversation::Usage;
use respan_gateway_conversation_http::fields::{base64_bytes, invalid, merge_missing};
use respan_gateway_core::{Connection, Fault, Format, Native, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::HeaderValue;
use respan_gateway_http::http::header::CONTENT_TYPE;
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::{Connect, Encoder, KeyIn, ProviderIo, Upstream, Whole, native::Json};
use respan_gateway_media::speech::{Input, Output};
use serde_json::{Map, Value, json};

const NAME: &str = "gemini-speech";

type Key = crate::Own<GeminiSpeech>;

/// Gemini's speech models (`gemini-3.8-flash-tts`) as the Gemini API serves
/// them: a `POST` to `/interactions`, its answer's audio the speech.
///
/// A call is one user turn of the text, with `instructions` as its
/// `speech_metadata` `style`, a `voice`, if any, as a prebuilt voice of the
/// `speech_config`, and the audio format as the `response_format`'s
/// `mime_type`: `mp3` (unless asked), `wav` or `pcm` (`audio/l16`, 16-bit
/// at 24 kHz). Another format, a `speed`, which Gemini has no field
/// for, and `instructions` with no voice, which Gemini refuses, are refused,
/// 400, before the call. Its native fields go in the body.
///
/// An answer's audio is its `model_output` steps' `audio` contents, base64,
/// its usage `usage`'s input and output tokens (thoughts included), the rest
/// of it, its tokens by modality, an [`Output::Native`] `{"usage": {..}}`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GeminiSpeech;

impl GeminiSpeech {
    /// The format's name, `gemini-speech`.
    pub const NAME: &'static str = NAME;
}

/// Its spec's: the key in `x-goog-api-key`, over HTTP.
impl Upstream for GeminiSpeech {
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

impl Format<ProviderIo, Output, Input> for GeminiSpeech {
    fn name(&self) -> &str {
        NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        Ok(bind_provider(io, NAME, post, |_, _| {
            Some(Box::new(Whole::new(read_response)))
        }))
    }
}

/// The call of `input`.
fn post(head: &mut RequestHead, input: &Input) -> Result<Box<dyn Encoder<Input>>, Fault> {
    if input.speed.is_some() {
        return Err(invalid("speed", "is not taken by Gemini speech"));
    }
    if input.instructions.is_some() && input.voice.is_none() {
        return Err(invalid(
            "instructions",
            "need a voice, which Gemini asks of a style",
        ));
    }
    let mime = match input.audio_format.as_deref().unwrap_or("mp3") {
        "mp3" => "audio/mp3",
        "wav" => "audio/wav",
        "pcm" => "audio/l16",
        other => {
            let what = format!("{other} is not written by Gemini: mp3, wav or pcm");
            return Err(invalid("response_format", &what));
        }
    };
    respan_gateway_http::path::set_path(head, "/interactions")?;
    head.headers
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    let mut text = json!({ "type": "text", "text": input.text });
    if let Some(style) = &input.instructions {
        text["annotations"] = json!([{ "type": "speech_metadata", "style": style }]);
    }
    let mut body = Map::new();
    if let Some(model) = &input.model {
        body.insert("model".into(), model.clone().into());
    }
    body.insert(
        "input".into(),
        json!([{ "type": "user_input", "content": [text] }]),
    );
    body.insert(
        "response_format".into(),
        json!({ "type": "audio", "mime_type": mime }),
    );
    if let Some(voice) = &input.voice {
        body.insert(
            "generation_config".into(),
            json!({ "speech_config": [{ "voice": voice }] }),
        );
    }
    merge_missing(&mut body, input.native.fields::<Key>());
    let body = serde_json::to_vec(&body)
        .map_err(|e| Fault::internal("a body could not be written").with_source(e))?;
    Ok(sends(Bytes::from(body)))
}

/// The events of an answer: its audio, in order, and its usage.
fn read_response(raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
    let v = serde_json::from_slice::<Value>(&raw)
        .map_err(|_| Fault::upstream("Gemini's speech answer is not JSON"))?;
    let steps = v["steps"].as_array().into_iter().flatten();
    let contents = steps.flat_map(|s| s["content"].as_array().into_iter().flatten());
    let mut heard = false;
    for audio in contents.filter(|c| c["type"] == "audio") {
        let data = audio["data"].as_str().and_then(base64_bytes);
        let data =
            data.ok_or_else(|| Fault::upstream("Gemini's speech holds audio not in base64"))?;
        out.push(Output::Audio(Bytes::from(data)));
        heard = true;
    }
    if !heard {
        return Err(Fault::upstream("Gemini's speech answer holds no audio"));
    }
    if let Some(usage) = v["usage"].as_object() {
        let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
        let output = count("total_output_tokens").saturating_add(count("total_thought_tokens"));
        let tokens = Usage::new(count("total_input_tokens"), output);
        if !tokens.is_empty() {
            out.push(Output::Usage(tokens));
        }
        let mut extras = usage.clone();
        for key in [
            "total_input_tokens",
            "total_output_tokens",
            "total_thought_tokens",
            "total_tokens",
        ] {
            extras.remove(key);
        }
        if !extras.is_empty() {
            let extras = json!({ "usage": extras });
            out.push(Output::Native(Native::json::<Key>(extras)));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_http::http::Request;

    fn call(input: &Input) -> Result<(RequestHead, Value), Fault> {
        let (mut head, ()) = Request::new(()).into_parts();
        let mut encoder = post(&mut head, input)?;
        let mut out = Vec::new();
        encoder.encode(input, &mut out)?;
        let body = serde_json::from_slice(&out.concat()).expect("a JSON body");
        Ok((head, body))
    }

    #[test]
    fn a_call_is_one_turn_of_text_in_its_voice_and_style() {
        let mut input = Input::new("Hello");
        input.model = Some("gemini-3.8-flash-tts".into());
        input.voice = Some("Kore".into());
        input.instructions = Some("cheerful".into());
        input.audio_format = Some("pcm".into());
        let (head, body) = call(&input).expect("a call");
        assert_eq!(head.uri, "/interactions");
        assert_eq!(
            body,
            json!({
                "model": "gemini-3.8-flash-tts",
                "input": [{"type": "user_input", "content": [{"type": "text", "text": "Hello",
                    "annotations": [{"type": "speech_metadata", "style": "cheerful"}]}]}],
                "response_format": {"type": "audio", "mime_type": "audio/l16"},
                "generation_config": {"speech_config": [{"voice": "Kore"}]},
            })
        );
    }

    #[test]
    fn an_answer_is_its_audio_and_the_tokens_it_counts() {
        let answer = json!({"steps": [{"type": "model_output", "content": [
            {"type": "audio", "data": "SUQz", "mime_type": "audio/mp3"}]}],
            "usage": {"total_input_tokens": 9, "total_output_tokens": 66, "total_tokens": 75,
                "output_tokens_by_modality": [{"modality": "audio", "tokens": 66}]}});
        let mut out = Vec::new();
        read_response(Bytes::from(answer.to_string()), &mut out).expect("an answer");
        assert_eq!(out[0], Output::Audio(Bytes::from_static(b"ID3")));
        assert_eq!(out[1], Output::Usage(Usage::new(9, 66)));
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn a_speed_a_format_and_a_style_without_a_voice_are_refused() {
        let mut input = Input::new("Hello");
        input.speed = Some(1.5);
        assert!(call(&input).is_err());
        input.speed = None;
        input.audio_format = Some("opus".into());
        assert!(call(&input).is_err());
        input.audio_format = None;
        input.instructions = Some("calm".into());
        assert!(call(&input).is_err());
    }
}
