//! Soniox's REST TTS protocol, translated to the speech kind.

use crate::edge::{bind_provider, sends};
use crate::openai::OpenAiSpeech;
use respan_gateway_conversation_http::fields::{invalid, kept};
use respan_gateway_core::{Connection, Fault, Format, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::{HeaderValue, header::CONTENT_TYPE, request::Parts};
use respan_gateway_http::{Connect, Decoder, Encoder, ProviderIo, Upstream};
use respan_gateway_media::speech::{Input, Output};
use serde_json::{Value, json};

/// Soniox REST TTS: a bearer-authenticated `POST /tts`, returning raw audio.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SonioxSpeech;

impl SonioxSpeech {
    /// The native provider format's name.
    pub const NAME: &'static str = "soniox-speech";
}

impl Upstream for SonioxSpeech {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |extensions| connect(extensions))
    }
}

impl Format<ProviderIo, Output, Input> for SonioxSpeech {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        Ok(bind_provider(io, Self::NAME, post, |_, _| {
            Some(Box::new(|bytes: Bytes, out: &mut Vec<Output>| {
                out.push(Output::Audio(bytes));
                Ok(())
            }) as Box<dyn Decoder<Output>>)
        }))
    }
}

fn post(head: &mut Parts, input: &Input) -> Result<Box<dyn Encoder<Input>>, Fault> {
    // Only language has a defined translation from OpenAI's extra fields.
    let native_format = match &input.native {
        Some(native) if &*native.format == OpenAiSpeech::NAME => OpenAiSpeech::NAME,
        _ => SonioxSpeech::NAME,
    };
    let native = kept(native_format, &input.native)?;
    if let Some(name) = native.keys().find(|name| name.as_str() != "language") {
        return Err(invalid(name, "is not supported by Soniox speech"));
    }
    let required = |value: Option<&str>, name: &str| {
        value
            .filter(|s| !s.trim().is_empty() && s.len() <= 50)
            .ok_or_else(|| invalid(name, "must be a nonempty string of at most 50 bytes"))
            .map(str::to_owned)
    };
    let model = required(input.model.as_deref(), "model")?;
    let voice = required(input.voice.as_deref(), "voice")?;
    let language = required(native.get("language").and_then(Value::as_str), "language")?;
    if input.text.is_empty() || input.text.chars().count() > 5000 {
        return Err(invalid(
            "input",
            "must contain between 1 and 5000 characters",
        ));
    }
    if input.instructions.is_some() {
        return Err(invalid("instructions", "is not supported by Soniox speech"));
    }
    if input
        .speed
        .is_some_and(|speed| !(0.7..=1.3).contains(&speed))
    {
        return Err(invalid("speed", "must be between 0.7 and 1.3"));
    }
    let format = input.audio_format.as_deref().unwrap_or("mp3");
    let format = match format {
        "mp3" | "wav" => format,
        "pcm" => "pcm_s16le",
        _ => return Err(invalid("response_format", "must be mp3, wav, or pcm")),
    };
    let mut body = json!({
        "model": model, "voice": voice, "language": language,
        "audio_format": format, "text": input.text,
    });
    if let Some(speed) = input.speed {
        body["speed"] = speed.into();
    }
    if format == "pcm_s16le" {
        body["sample_rate"] = 24000.into();
    }
    respan_gateway_http::path::set_path(head, "/tts")?;
    head.headers
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    let body = serde_json::to_vec(&body).map_err(|error| {
        Fault::internal("Soniox speech JSON could not be encoded").with_source(error)
    })?;
    Ok(sends(body.into()))
}

#[cfg(test)]
mod tests;
