//! Gemini's speech models, through `generateContent`: a format of the
//! [`speech`](respan_gateway_media::speech) kind, [`GeminiSpeech`].

use super::is_segment;
use crate::dialect::{self, Spec};
use crate::edge::sends;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use respan_gateway_conversation_http::fields::{invalid, kept};
use respan_gateway_conversation_http::gemini::usage_of;
use respan_gateway_core::Connection;
use respan_gateway_core::{ErrorCode, Fault, Format, Provider};
use respan_gateway_http::bytes::{BufMut, Bytes, BytesMut};
use respan_gateway_http::http::header::CONTENT_TYPE;
use respan_gateway_http::http::{
    self, HeaderValue, request::Parts as RequestHead, response::Parts as ResponseHead,
};
use respan_gateway_http::{Connect, Decoder, Encoder, KeyIn, ProviderIo, Upstream, Whole};
use respan_gateway_media::speech::{Input, Output};
use serde_json::{Map, Value, json};

const NAME: &str = "gemini-speech";

/// Gemini's speech models (`gemini-3.8-flash-tts`), as the Gemini API
/// serves them: a `POST` to `.../models/{model}:generateContent` of one
/// user turn, the text to speak, asking for audio
/// (`responseModalities: ["AUDIO"]`) in the voice named
/// (`speechConfig.voiceConfig.prebuiltVoiceConfig.voiceName`), such as
/// `Kore`. Instructions, which it takes in the text, lead it:
/// `{instructions}: {text}`.
///
/// It speaks a `wav`: 16-bit PCM, mono, at the rate it says (24 kHz), with
/// the header put before the samples if it answers them bare
/// (`audio/L16`). Another audio format, or none (a client of OpenAI's
/// speech asks `mp3` by not saying), is refused, 400 `unsupported`, before
/// any call, as is a `speed`, which it does not take. The native fields of this format go in the body,
/// `generationConfig` merged with what it says. An answer with no audio,
/// saying why, is refused, 400 `no_audio`.
///
/// It serves providers only: no client speaks it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GeminiSpeech;

impl GeminiSpeech {
    /// The format's name, `gemini-speech`, which tags its native fields.
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
        Ok(dialect::bind(*self, io))
    }
}

/// Its spec's parts, which a vendor's dialect runs around.
impl Spec for GeminiSpeech {
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

/// The call of `input`: one user turn of its text, asking for audio.
fn call(head: &mut RequestHead, input: &Input) -> Result<Box<dyn Encoder<Input>>, Fault> {
    let model = input.model.as_deref().unwrap_or_default();
    let model = model.strip_prefix("models/").unwrap_or(model);
    if !is_segment(model) {
        return Err(Fault::bad_request(
            "Gemini speech needs a model, such as gemini-3.8-flash-tts",
        ));
    }
    match input.audio_format.as_deref() {
        Some("wav") => {}
        said => {
            let what = format!(
                "is {}; {NAME} speaks `wav`",
                said.map_or("not given, so `mp3`".to_owned(), |f| format!("`{f}`"))
            );
            return Err(invalid("response_format", &what).with(ErrorCode("unsupported".into())));
        }
    }
    if input.speed.is_some() {
        return Err(invalid("speed", &format!("is not one {NAME} takes"))
            .with(ErrorCode("unsupported".into())));
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
    let mut config = match native.remove("generationConfig") {
        Some(Value::Object(config)) => config,
        None => Map::new(),
        Some(_) => return Err(invalid("generationConfig", "must be an object")),
    };
    config.insert("responseModalities".into(), json!(["AUDIO"]));
    if let Some(voice) = &input.voice {
        let voice = json!({ "voiceConfig": { "prebuiltVoiceConfig": { "voiceName": voice } } });
        config.insert("speechConfig".into(), voice);
    }
    let text = match &input.instructions {
        Some(how) => format!("{how}: {}", input.text),
        None => input.text.clone(),
    };
    let mut v = Map::new();
    v.insert(
        "contents".into(),
        json!([{ "role": "user", "parts": [{ "text": text }] }]),
    );
    v.insert("generationConfig".into(), config.into());
    for (key, value) in native {
        v.entry(key).or_insert(value);
    }
    Ok(v)
}

/// The events of an answer: its audio, as a `wav`, then its usage. One
/// with no audio is refused, saying why.
fn read_response(raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
    let v = serde_json::from_slice::<Value>(&raw)
        .map_err(|_| Fault::upstream(format!("the provider's {NAME} response is not JSON")))?;
    let candidate = &v["candidates"][0];
    let parts = candidate["content"]["parts"]
        .as_array()
        .into_iter()
        .flatten();
    let audio = parts.filter_map(|p| p.get("inlineData")).find(|d| {
        d["mimeType"]
            .as_str()
            .is_some_and(|m| m.starts_with("audio/"))
    });
    let Some(audio) = audio else {
        let why = v["promptFeedback"]["blockReason"]
            .as_str()
            .or(candidate["finishReason"].as_str())
            .unwrap_or("no reason given");
        let message = format!("Gemini spoke no audio ({why})");
        return Err(Fault::bad_request(message).with(ErrorCode("no_audio".into())));
    };
    let bytes = STANDARD
        .decode(audio["data"].as_str().unwrap_or_default())
        .map_err(|_| Fault::upstream(format!("the provider's {NAME} audio is not base64")))?;
    let mime = audio["mimeType"].as_str().unwrap_or_default();
    let wav = match bytes.starts_with(b"RIFF") {
        true => Bytes::from(bytes),
        false => {
            let rate = rate_of(mime).ok_or_else(|| {
                Fault::upstream(format!("the provider's {NAME} audio is {mime}, not PCM"))
            })?;
            wav_of(&bytes, rate)
        }
    };
    out.push(Output::Audio(wav));
    if v["usageMetadata"].is_object() {
        out.push(Output::Usage(usage_of(&v["usageMetadata"])?));
    }
    Ok(())
}

/// The rate of bare 16-bit PCM of `mime`, `audio/L16` at its `rate=`.
fn rate_of(mime: &str) -> Option<u32> {
    let mime = mime.to_ascii_lowercase();
    if !mime.starts_with("audio/l16") && !mime.starts_with("audio/pcm") {
        return None;
    }
    let rate = mime.split(';').find_map(|p| p.trim().strip_prefix("rate="));
    Some(rate.and_then(|r| r.parse().ok()).unwrap_or(24_000))
}

/// `samples`, 16-bit mono PCM at `rate`, as a `wav`.
fn wav_of(samples: &[u8], rate: u32) -> Bytes {
    let size = u32::try_from(samples.len()).unwrap_or(u32::MAX);
    let mut wav = BytesMut::with_capacity(44 + samples.len());
    wav.put_slice(b"RIFF");
    wav.put_u32_le(size.saturating_add(36));
    wav.put_slice(b"WAVEfmt ");
    wav.put_u32_le(16);
    wav.put_u16_le(1);
    wav.put_u16_le(1);
    wav.put_u32_le(rate);
    wav.put_u32_le(rate * 2);
    wav.put_u16_le(2);
    wav.put_u16_le(16);
    wav.put_slice(b"data");
    wav.put_u32_le(size);
    wav.put_slice(samples);
    wav.freeze()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(format: Option<&str>) -> Input {
        let mut input = Input::new("Hello.");
        input.model = Some("gemini-3.8-flash-tts".into());
        input.audio_format = format.map(str::to_owned);
        input
    }

    fn sent(input: &Input) -> Result<(RequestHead, Value), Fault> {
        let (mut head, ()) = http::Request::new(()).into_parts();
        let mut encoder = call(&mut head, input)?;
        let mut out = Vec::new();
        encoder.encode(input, &mut out)?;
        let body = serde_json::from_slice(&out.concat()).unwrap_or_default();
        Ok((head, body))
    }

    #[test]
    fn a_call_asks_for_audio_in_the_voice_with_its_instructions_leading() {
        let mut i = input(Some("wav"));
        i.voice = Some("Kore".into());
        i.instructions = Some("Say cheerfully".into());
        let (head, body) = sent(&i).expect("sent");
        assert_eq!(
            head.uri.to_string(),
            "/models/gemini-3.8-flash-tts:generateContent"
        );
        assert_eq!(
            body["contents"][0]["parts"][0]["text"],
            "Say cheerfully: Hello."
        );
        let config = &body["generationConfig"];
        assert_eq!(config["responseModalities"], json!(["AUDIO"]));
        assert_eq!(
            config["speechConfig"]["voiceConfig"]["prebuiltVoiceConfig"]["voiceName"],
            "Kore"
        );
    }

    #[test]
    fn a_format_but_wav_and_a_speed_are_refused() {
        for format in [None, Some("mp3"), Some("pcm")] {
            assert!(sent(&input(format)).is_err(), "{format:?}");
        }
        let mut fast = input(Some("wav"));
        fast.speed = Some(1.5);
        assert!(sent(&fast).is_err());
    }

    fn answer(mime: &str, audio: &[u8]) -> Bytes {
        let v = json!({
            "candidates": [{"content": {"parts": [
                {"inlineData": {"mimeType": mime, "data": STANDARD.encode(audio)}},
            ]}, "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 4, "candidatesTokenCount": 9, "totalTokenCount": 13},
        });
        Bytes::from(v.to_string())
    }

    #[test]
    fn its_audio_is_a_wav_whichever_it_answers() {
        let samples = [1u8, 0, 2, 0];
        let wav = wav_of(&samples, 24_000);
        for (mime, raw) in [
            ("audio/wav", wav.as_ref()),
            ("audio/L16;codec=pcm;rate=24000", &samples[..]),
        ] {
            let mut out = Vec::new();
            read_response(answer(mime, raw), &mut out).expect("read");
            assert_eq!(out[0], Output::Audio(wav.clone()), "{mime}");
            assert!(matches!(&out[1], Output::Usage(u) if u.output_tokens == 9));
        }
        let mut out = Vec::new();
        let none = json!({"candidates": [{"content": {"parts": []}, "finishReason": "OTHER"}]});
        let refused = read_response(Bytes::from(none.to_string()), &mut out);
        assert!(refused.is_err_and(|f| f.message().contains("OTHER")));
    }
}
