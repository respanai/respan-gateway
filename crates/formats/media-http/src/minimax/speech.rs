//! MiniMax's speech synthesis, `POST /t2a_v2`: a format of the
//! [`speech`](respan_gateway_media::speech) kind, [`MiniMaxSpeech`].

use crate::edge::{bind_provider, ended_events, posts};
use respan_gateway_conversation_http::fields::{invalid, merge_missing, set};
use respan_gateway_conversation_http::read_provider_error;
use respan_gateway_core::{Connection, Fault, Format, Native, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::header::CONTENT_TYPE;
use respan_gateway_http::http::{self, StatusCode, request::Parts as RequestHead};
use respan_gateway_http::{Connect, Decoder, Encoder, ProviderIo, Upstream, Whole, native::Json};
use respan_gateway_media::speech::{Input, Output};
use serde_json::{Map, Value, json};

const NAME: &str = "minimax-speech";

type Key = crate::Own<MiniMaxSpeech>;
/// The audio formats MiniMax writes, of those a client names.
const FORMATS: [&str; 5] = ["mp3", "pcm", "flac", "wav", "opus"];

/// MiniMax's speech synthesis on its international API (`speech-2.8-hd`): a
/// `POST` of JSON to `/t2a_v2`, asked for the audio in hex, never as a URL.
///
/// The text and model, the voice (as `voice_setting.voice_id`), speed (as
/// `voice_setting.speed`) and audio format (as `audio_setting.format`) are
/// its body, with the input's native fields: `voice_setting` and
/// `audio_setting` ones, such as `vol`, `pitch` and `sample_rate`, join
/// theirs, the rest, such as `language_boost`, sit at the top. A voice is
/// needed; an audio format of `mp3` (unless asked), `pcm`, `flac`, `wav` or
/// `opus` (`pcm` and `opus` at 24 kHz unless a `sample_rate` is native, as
/// MiniMax refuses Opus at its own 32 kHz), and no `instructions`: another
/// is refused, 400, before the call. Speech asked as `mp3`, `pcm` or `opus`
/// is streamed; `wav` (whose streamed header has no length) and `flac` are
/// asked whole.
///
/// A streamed answer's events each hold a hex `data.audio` chunk, decoded
/// into an [`Output::Audio`] chunk, in order; the event that ends it
/// (`data.status: 2`) repeats the whole audio unless asked not to, and is
/// asked not to (`stream_options.exclude_aggregated_audio`), its own audio,
/// if any, left out. A whole answer's `data.audio` is the one chunk. The
/// `extra_info` (the characters billed, the audio's length) is an
/// [`Output::Native`] `{"extra_info": {..}}`, and a stream cut before its
/// end fails `truncated`. MiniMax answers an error as a success with a
/// `base_resp.status_code` but 0: it is the fault it says, a 400 for a
/// parameter refused, 401, 429, or else a 502.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MiniMaxSpeech;

impl MiniMaxSpeech {
    /// The format's name, `minimax-speech`.
    pub const NAME: &'static str = NAME;
}

/// A bearer key, over HTTP, beside its base URL.
impl Upstream for MiniMaxSpeech {
    fn name(&self) -> &str {
        NAME
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for MiniMaxSpeech {
    fn name(&self) -> &str {
        NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        Ok(bind_provider(io, NAME, post, |head, sent| {
            // A stream refused is plain JSON, not events.
            let json = head.headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok());
            let sent: Value = serde_json::from_slice(sent.body()).unwrap_or_default();
            Some(
                match sent["stream"] == true && !json.is_some_and(|t| t.contains("json")) {
                    true => ended_events(stream_event),
                    false => whole(),
                },
            )
        }))
    }
}

/// The call of `input`.
fn post(head: &mut RequestHead, input: &Input) -> Result<Box<dyn Encoder<Input>>, Fault> {
    let voice = input.voice.clone();
    let voice = voice.ok_or_else(|| invalid("voice", "is needed by MiniMax"))?;
    if input.instructions.is_some() {
        return Err(invalid("instructions", "is not taken by MiniMax"));
    }
    let format = input.audio_format.as_deref().unwrap_or("mp3");
    if !FORMATS.contains(&format) {
        return Err(invalid(
            "response_format",
            &format!("{format} is not written by MiniMax: mp3, pcm, flac, wav or opus"),
        ));
    }
    let mut native = input.native.fields::<Key>().clone();
    let mut section = |name: &str| match native.remove(name) {
        Some(Value::Object(fields)) => fields,
        _ => Map::new(),
    };
    let mut voice_setting = section("voice_setting");
    let mut audio_setting = section("audio_setting");
    voice_setting.insert("voice_id".into(), voice.into());
    set(&mut voice_setting, "speed", input.speed);
    audio_setting.insert("format".into(), format.into());
    if matches!(format, "pcm" | "opus") {
        // MiniMax's own 32 kHz is refused for Opus, and is not OpenAI's 24 kHz PCM.
        audio_setting.entry("sample_rate").or_insert(24000.into());
    }
    let stream = matches!(format, "mp3" | "pcm" | "opus");
    let mut v = Map::new();
    set(&mut v, "model", input.model.clone());
    v.insert("text".into(), input.text.clone().into());
    v.insert("stream".into(), stream.into());
    v.insert("output_format".into(), "hex".into());
    v.insert("voice_setting".into(), voice_setting.into());
    v.insert("audio_setting".into(), audio_setting.into());
    if stream {
        v.insert(
            "stream_options".into(),
            json!({ "exclude_aggregated_audio": true }),
        );
    }
    merge_missing(&mut v, &native);
    posts(head, "/t2a_v2", &v)
}

/// The decoder of MiniMax's whole answer.
fn whole() -> Box<dyn Decoder<Output>> {
    Box::new(Whole::new(|raw: Bytes, out: &mut Vec<Output>| {
        let v: Value = serde_json::from_slice(&raw)
            .map_err(|_| Fault::upstream("MiniMax's speech answer is not JSON"))?;
        read(&v, true, out)?;
        if out.is_empty() {
            return Err(Fault::upstream("MiniMax's speech answer holds no audio"));
        }
        Ok(())
    }))
}

/// The events of one stream event; `done` once it said its end.
fn stream_event(done: &mut bool, raw: &[u8], out: &mut Vec<Output>) -> Result<(), Fault> {
    let Some(data) = respan_gateway_sse::event_data(raw) else {
        return Ok(());
    };
    let v: Value = serde_json::from_str(&data)
        .map_err(|_| Fault::upstream("MiniMax's speech event is not JSON"))?;
    // The last event repeats the audio before it, if not asked to leave it out.
    let last = v["data"]["status"] == 2;
    *done |= last;
    read(&v, !last, out)
}

/// The events of an answer `v`: its audio, in hex, if `audio`, then its
/// `extra_info`; the fault of an error it says.
fn read(v: &Value, audio: bool, out: &mut Vec<Output>) -> Result<(), Fault> {
    let status = v["base_resp"]["status_code"].as_i64().unwrap_or(0);
    if status != 0 {
        return Err(said_error(status, v));
    }
    let hex = v["data"]["audio"]
        .as_str()
        .filter(|a| audio && !a.is_empty());
    if let Some(audio) = hex {
        let audio = hex_bytes(audio)
            .ok_or_else(|| Fault::upstream("MiniMax's speech answer holds audio not in hex"))?;
        out.push(Output::Audio(Bytes::from(audio)));
    }
    if v["extra_info"].is_object() {
        let info = json!({ "extra_info": v["extra_info"] });
        out.push(Output::Native(Native::json::<Key>(info)));
    }
    Ok(())
}

/// The bytes of `hex`, if it is hex.
fn hex_bytes(hex: &str) -> Option<Vec<u8>> {
    let digit = |b: u8| char::from(b).to_digit(16).map(|d| d as u8);
    let hex = hex.as_bytes();
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    hex.chunks(2)
        .map(|pair| Some(digit(pair[0])? << 4 | digit(pair[1])?))
        .collect()
}

/// The fault of an error MiniMax said in a success, its
/// `base_resp.status_code` `code`: a 400 for a parameter it refused, 401 for
/// a key, 402 for a balance, 429 for a limit, else a 502.
fn said_error(code: i64, v: &Value) -> Fault {
    let mut answer = http::Response::new(Bytes::from(
        json!({ "code": code.to_string(), "message": v["base_resp"]["status_msg"] }).to_string(),
    ));
    *answer.status_mut() = match code {
        2013 | 1042 | 2054 => StatusCode::BAD_REQUEST,
        1004 | 2049 => StatusCode::UNAUTHORIZED,
        1008 => StatusCode::PAYMENT_REQUIRED,
        1002 | 1039 => StatusCode::TOO_MANY_REQUESTS,
        _ => StatusCode::BAD_GATEWAY,
    };
    read_provider_error(answer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Natives;
    use respan_gateway_core::FaultKind;

    fn event(v: Value) -> Vec<u8> {
        format!("data: {v}\n\n").into_bytes()
    }

    fn status(fault: &Fault) -> Option<StatusCode> {
        let status = fault.get::<respan_gateway_http_faults::Status>();
        status.map(|s| s.0)
    }

    fn sent(input: &Input) -> Result<Value, Fault> {
        let mut head = http::Request::new(()).into_parts().0;
        let mut out = Vec::new();
        post(&mut head, input)?.encode(input, &mut out)?;
        Ok(serde_json::from_slice(&out.concat()).expect("a JSON body"))
    }

    #[test]
    fn a_call_is_the_voice_speed_and_format_asked_for_in_hex() {
        let mut input = Input::new("hi");
        input.model = Some("speech-2.8-hd".into());
        input.voice = Some("narrator".into());
        input.speed = Some(1.5);
        input.audio_format = Some("pcm".into());
        input.native = MiniMaxSpeech::with_fields(&Native::default(), |f| {
            f.insert("voice_setting".into(), json!({ "vol": 2 }));
            f.insert("output_format".into(), "url".into());
        });
        let v = sent(&input).expect("a call");
        assert_eq!(
            v["voice_setting"],
            json!({"voice_id": "narrator", "speed": 1.5, "vol": 2})
        );
        assert_eq!(
            v["audio_setting"],
            json!({"format": "pcm", "sample_rate": 24000})
        );
        assert_eq!(
            (v["stream"].clone(), v["output_format"].clone()),
            (true.into(), "hex".into())
        );
        input.audio_format = Some("wav".into());
        assert_eq!(sent(&input).expect("a call")["stream"], false);
    }

    #[test]
    fn a_stream_is_its_audio_in_order_and_not_the_last_events_copy() {
        let mut done = false;
        let mut out = Vec::new();
        let info = json!({"usage_characters": 2});
        let events = [
            json!({"data": {"audio": "494433", "status": 1}, "base_resp": {"status_code": 0}}),
            json!({"data": {"audio": "494433", "status": 2}, "extra_info": info,
                "base_resp": {"status_code": 0}}),
        ];
        for v in events {
            stream_event(&mut done, &event(v), &mut out).expect("an event");
        }
        assert!(done);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], Output::Audio(Bytes::from_static(b"ID3")));
        let Output::Native(native) = &out[1] else {
            panic!("the extra info is native");
        };
        assert_eq!(native.fields::<Key>()["extra_info"], info);
    }

    #[test]
    fn an_error_in_a_success_is_the_fault_it_says() {
        let v = json!({"base_resp": {"status_code": 2013, "status_msg": "invalid params"}});
        let fault = read(&v, true, &mut Vec::new()).expect_err("a fault");
        assert_eq!(status(&fault), Some(StatusCode::BAD_REQUEST));
        assert_eq!(fault.message(), "invalid params");
        let key = json!({"base_resp": {"status_code": 2049, "status_msg": "invalid api key"}});
        let fault = read(&key, true, &mut Vec::new()).expect_err("a fault");
        assert_eq!(status(&fault), Some(StatusCode::UNAUTHORIZED));
        assert!(hex_bytes("4g").is_none() && hex_bytes("494").is_none());
    }

    #[test]
    fn what_minimax_cannot_do_is_refused_before_the_call() {
        let mut input = Input::new("hi");
        input.voice = Some("narrator".into());
        input.instructions = Some("softly".into());
        assert!(refused(&input));
        input.instructions = None;
        input.audio_format = Some("aac".into());
        assert!(refused(&input));
        input.audio_format = None;
        input.voice = None;
        assert!(refused(&input));
    }

    fn refused(input: &Input) -> bool {
        sent(input).expect_err("a refusal").is(FaultKind::Client)
    }
}
