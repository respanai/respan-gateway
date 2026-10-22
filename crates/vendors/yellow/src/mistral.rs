//! Mistral: Chat Completions, embeddings, transcriptions and speech, as
//! OpenAI serves them, speech in its dialect, and its own realtime
//! transcription.

use std::borrow::Cow;

use base64::Engine as _;
use respan_gateway_conversation_http::fields::{kept, native_of};
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::Fault;
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::content::content_type;
use respan_gateway_http::http::response::Parts as ResponseHead;
use respan_gateway_http::{Decoder, Whole};
use respan_gateway_media::speech;
use respan_gateway_media_http::dialect::{Dialect, Dialected};
use respan_gateway_media_http::openai::{OpenAiEmbeddings, OpenAiSpeech, OpenAiTranscriptions};
use respan_gateway_realtime_ws::MistralRealtime;
use serde_json::Value;

use respan_gateway_vendor_kit::Vendor;

/// Where its API is.
const BASE_URL: &str = "https://api.mistral.ai/v1";

/// Where its realtime transcription is.
const REALTIME_URL: &str = "wss://api.mistral.ai/v1";

/// Mistral.
pub const VENDOR: Vendor = Vendor {
    id: "mistral",
    key_env: &["MISTRAL_API_KEY"],
    formats: &[
        (&OpenAiChat, Some(BASE_URL)),
        (&OpenAiEmbeddings, Some(BASE_URL)),
        (&OpenAiTranscriptions, Some(BASE_URL)),
        (&Dialected::new(OpenAiSpeech, Mistral), Some(BASE_URL)),
        (&MistralRealtime, Some(REALTIME_URL)),
    ],
    ..Vendor::DEFAULT
};

/// Mistral's speech: it takes the voice as `voice_id`, refuses (422)
/// `instructions` and `speed`, which fail `bad_request` before sending, and
/// answers every `response_format` as JSON, `{"audio_data": <base64>}`,
/// read whole within the body's limit and decoded into one
/// [`speech::Output::Audio`]; an answer of audio is read as the spec's. It
/// is not asked to stream, as its events are not OpenAI's; a client that
/// asked still gets a stream, framed at the client edge. Its `pcm` is
/// 32-bit float at 24 kHz, not OpenAI's 16-bit.
#[derive(Debug, Clone, Copy)]
pub struct Mistral;

impl Dialect<speech::Input, speech::Output> for Mistral {
    fn input<'a>(&self, input: &'a speech::Input) -> Result<Cow<'a, speech::Input>, Fault> {
        if input.instructions.is_some() {
            return Err(Fault::bad_request("mistral speech takes no 'instructions'"));
        }
        if input.speed.is_some() {
            return Err(Fault::bad_request("mistral speech takes no 'speed'"));
        }
        let Some(voice) = &input.voice else {
            return Ok(Cow::Borrowed(input));
        };
        let mut fields = kept(OpenAiSpeech::NAME, &input.native)?;
        fields
            .entry("voice_id")
            .or_insert_with(|| voice.clone().into());
        let mut input = input.clone();
        input.voice = None;
        input.native = native_of(OpenAiSpeech::NAME, fields);
        Ok(Cow::Owned(input))
    }

    fn read_response(&self, head: &ResponseHead) -> Option<Box<dyn Decoder<speech::Output>>> {
        let essence = content_type(head)?.split(';').next()?.trim();
        let json = essence.eq_ignore_ascii_case("application/json");
        (json && head.status.is_success()).then(|| Box::new(Whole::new(audio_data)) as _)
    }

    fn streams(&self, _: bool) -> bool {
        false
    }
}

/// The audio of a whole answer: its `audio_data`, decoded.
fn audio_data(raw: Bytes, out: &mut Vec<speech::Output>) -> Result<(), Fault> {
    let answer: Value = serde_json::from_slice(&raw)
        .map_err(|_| Fault::upstream("mistral speech answered no JSON"))?;
    let audio = answer
        .get("audio_data")
        .and_then(Value::as_str)
        .and_then(|data| base64::engine::general_purpose::STANDARD.decode(data).ok())
        .ok_or_else(|| Fault::upstream("mistral speech answered no base64 'audio_data'"))?;
    out.push(speech::Output::Audio(Bytes::from(audio)));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_http::http;

    fn head(content_type: &str) -> ResponseHead {
        let answer = http::Response::builder().header("content-type", content_type);
        answer.body(()).expect("head").into_parts().0
    }

    fn read(decoder: &mut dyn Decoder<speech::Output>, body: &[u8]) -> Result<Vec<Bytes>, Fault> {
        let mut out = Vec::new();
        decoder.decode(Bytes::copy_from_slice(body), &mut out)?;
        decoder.end(Ok(()), &mut out)?;
        let audio = |o| match o {
            speech::Output::Audio(bytes) => Some(bytes),
            _ => None,
        };
        Ok(out.into_iter().filter_map(audio).collect())
    }

    #[test]
    fn json_answer_is_its_audio_data() {
        let mut decoder = Mistral
            .read_response(&head("application/json"))
            .expect("read");
        let audio = read(&mut *decoder, br#"{"audio_data": "SUQz"}"#).expect("audio");
        assert_eq!(audio, [Bytes::from_static(b"ID3")]);
        assert!(read(&mut *decoder, br#"{"audio": 1}"#).is_err());
    }

    #[test]
    fn json_answer_is_held_within_the_limit() {
        let mut decoder = Mistral
            .read_response(&head("application/json"))
            .expect("read");
        decoder.limit(Some(8));
        assert!(read(&mut *decoder, br#"{"audio_data": "SUQz"}"#).is_err());
    }

    #[test]
    fn audio_answer_is_the_specs() {
        assert!(Mistral.read_response(&head("audio/mpeg")).is_none());
    }
}
