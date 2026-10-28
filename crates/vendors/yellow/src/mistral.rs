//! Mistral: Chat Completions, embeddings, transcriptions and speech, as
//! OpenAI serves them, speech in its dialect, and its own realtime
//! transcription.

use base64::Engine as _;
use respan_gateway_conversation_http::fields::{kept, native_of};
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::{Fault, Filter};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::content::content_type;
use respan_gateway_http::http::header::{CONTENT_TYPE, HeaderValue};
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::http::response::Parts as ResponseHead;
use respan_gateway_media::speech;
use respan_gateway_media_http::openai::{OpenAiEmbeddings, OpenAiSpeech, OpenAiTranscriptions};
use respan_gateway_realtime_ws::MistralRealtime;
use serde_json::Value;

use respan_gateway_vendor_kit::{Dialect, Dialected, Rewrite, Vendor};

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

/// Mistral's speech: it takes the voice as `voice_id`, and refuses (422)
/// `instructions` and `speed`, which fail `bad_request` before sending. It
/// is not asked to stream, as its events are not OpenAI's; a client that
/// asked still gets a stream, framed at the client edge. It answers every
/// `response_format` as JSON, `{"audio_data": <base64>}`, read as the
/// audio it holds; an answer of audio is read as it is. Its `pcm` is
/// 32-bit float at 24 kHz, not OpenAI's 16-bit.
#[derive(Debug, Clone, Copy)]
pub struct Mistral;

impl Dialect for Mistral {
    type Input = speech::Input;
    type Output = speech::Output;

    fn inputs(&self) -> Option<Box<dyn Filter<speech::Input>>> {
        Some(Box::new(taken))
    }

    fn request(&self, _: &mut RequestHead, _: Option<&Bytes>) -> Result<Option<Rewrite>, Fault> {
        Ok(Some(Box::new(unstreamed)))
    }

    fn response(&self, _: &RequestHead, head: &mut ResponseHead) -> Option<Rewrite> {
        let essence = content_type(head)?.split(';').next()?.trim();
        if !head.status.is_success() || !essence.eq_ignore_ascii_case("application/json") {
            return None;
        }
        let audio = HeaderValue::from_static("application/octet-stream");
        head.headers.insert(CONTENT_TYPE, audio);
        Some(Box::new(audio_data))
    }
}

/// `input` as Mistral takes it: its voice as `voice_id`, and no
/// `instructions` or `speed`.
fn taken(input: &mut speech::Input) -> Result<(), Fault> {
    if input.instructions.is_some() {
        return Err(Fault::bad_request("mistral speech takes no 'instructions'"));
    }
    if input.speed.is_some() {
        return Err(Fault::bad_request("mistral speech takes no 'speed'"));
    }
    let Some(voice) = input.voice.take() else {
        return Ok(());
    };
    let mut fields = kept(OpenAiSpeech::NAME, &input.native)?;
    fields.entry("voice_id").or_insert_with(|| voice.into());
    input.native = native_of(OpenAiSpeech::NAME, fields);
    Ok(())
}

/// A call's JSON without its `stream_format`.
fn unstreamed(body: Bytes) -> Result<Bytes, Fault> {
    crate::without(body, "stream_format")
}

/// The audio of a whole answer: its `audio_data`, decoded.
fn audio_data(raw: Bytes) -> Result<Bytes, Fault> {
    let answer: Value = serde_json::from_slice(&raw)
        .map_err(|_| Fault::upstream("mistral speech answered no JSON"))?;
    let audio = answer
        .get("audio_data")
        .and_then(Value::as_str)
        .and_then(|data| base64::engine::general_purpose::STANDARD.decode(data).ok())
        .ok_or_else(|| Fault::upstream("mistral speech answered no base64 'audio_data'"))?;
    Ok(Bytes::from(audio))
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_http::http;

    fn head(content_type: &str) -> ResponseHead {
        let answer = http::Response::builder().header("content-type", content_type);
        answer.body(()).expect("head").into_parts().0
    }

    fn sent() -> RequestHead {
        http::Request::new(()).into_parts().0
    }

    #[test]
    fn json_answer_is_its_audio_data() {
        let mut json = head("application/json");
        let rewrite = Mistral.response(&sent(), &mut json).expect("a rewrite");
        assert_eq!(json.headers[CONTENT_TYPE], "application/octet-stream");
        let audio = rewrite(Bytes::from_static(br#"{"audio_data": "SUQz"}"#));
        assert_eq!(audio.expect("audio"), Bytes::from_static(b"ID3"));
        assert!(audio_data(Bytes::from_static(br#"{"audio": 1}"#)).is_err());
    }

    #[test]
    fn audio_answer_is_read_as_it_is() {
        assert!(Mistral.response(&sent(), &mut head("audio/mpeg")).is_none());
    }

    #[test]
    fn it_takes_a_voice_id_and_is_asked_for_no_stream() {
        let mut input = speech::Input::new("hi");
        input.voice = Some("v".into());
        taken(&mut input).expect("taken");
        assert_eq!(input.voice, None);
        let native = input.native.expect("native");
        assert_eq!(native.fields["voice_id"], "v");
        let mut asked = speech::Input::new("hi");
        asked.speed = Some(1.5);
        assert!(taken(&mut asked).is_err());
        let call = Bytes::from_static(br#"{"input":"hi","stream_format":"sse"}"#);
        assert_eq!(unstreamed(call).expect("a call"), &br#"{"input":"hi"}"#[..]);
    }
}
