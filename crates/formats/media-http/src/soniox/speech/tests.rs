use std::sync::{Arc, Mutex};

use super::SonioxSpeech;
use crate::openai::OpenAiSpeech;
use respan_gateway_core::Gateway;
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::{self, HeaderMap, HeaderValue, Request, StatusCode};
use respan_gateway_http::{Body, Upstream};
use respan_gateway_media::speech::{Input, Output};
use serde_json::{Value, json};

fn gateway(seen: Arc<Mutex<Vec<Value>>>, status: StatusCode) -> Gateway {
    let send = move |mut request: Request<Body>| {
        let seen = seen.clone();
        async move {
            let headers = HeaderMap::from_iter([(
                http::header::AUTHORIZATION,
                HeaderValue::from_static("Bearer soniox-test-key"),
            )]);
            respan_gateway_http::call::prepare_call(
                &mut request,
                "https://tts-rt.soniox.test",
                &headers,
            )?;
            assert_eq!(request.method(), http::Method::POST);
            assert_eq!(request.uri(), "https://tts-rt.soniox.test/tts");
            assert_eq!(request.headers()["authorization"], "Bearer soniox-test-key");
            assert_eq!(request.headers()["content-type"], "application/json");
            let body = request.into_body().read_answer(16384).await?;
            seen.lock()
                .expect("calls")
                .push(serde_json::from_slice(&body).expect("JSON"));
            let body = if status.is_success() {
                Body::stream(futures::stream::iter([
                    Ok(Bytes::from_static(b"audio-")),
                    Ok(Bytes::from_static(b"bytes")),
                ]))
            } else {
                Body::full(r#"{"error_type":"limit_exceeded","error_message":"slow down"}"#)
            };
            let answer = http::Response::builder()
                .status(status)
                .header("retry-after", "7")
                .header("content-type", "audio/mpeg")
                .body(body)
                .expect("response");
            Ok((answer, None))
        }
    };
    let connect = Arc::new(respan_gateway_http::connect(send, 16384));
    Gateway::new()
        .client::<Input, Output, _>(OpenAiSpeech)
        .expect("client")
        .provider(SonioxSpeech.provider("soniox".into(), connect))
        .expect("provider")
        .target("soniox")
        .expect("target")
}

async fn request(gateway: &Gateway, body: Value) -> (StatusCode, HeaderMap, Bytes) {
    let request = Request::post("/v1/audio/speech")
        .header("content-type", "application/json")
        .body(Body::full(body.to_string()))
        .expect("request");
    let (response, _) = respan_gateway_http::handle(gateway, request, 16384).await;
    let (head, body) = response.into_parts();
    (
        head.status,
        head.headers,
        body.read_answer(16384).await.expect("body"),
    )
}

fn input() -> Value {
    json!({"model":"tts-rt-v2","input":"Hello","voice":"Adrian","language":"en"})
}

#[tokio::test]
async fn an_openai_speech_request_becomes_soniox_json_and_raw_audio() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let gateway = gateway(seen.clone(), StatusCode::OK);
    let (status, headers, audio) = request(&gateway, input()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "audio/mpeg");
    assert_eq!(audio, "audio-bytes");
    assert_eq!(
        *seen.lock().expect("calls"),
        [json!({
            "model":"tts-rt-v2","text":"Hello","voice":"Adrian","language":"en","audio_format":"mp3"
        })]
    );
}

#[tokio::test]
async fn wav_and_fixed_24khz_pcm_use_the_requested_audio_format() {
    for (format, soniox, mime) in [
        ("wav", "wav", "audio/wav"),
        ("pcm", "pcm_s16le", "audio/pcm"),
    ] {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let gateway = gateway(seen.clone(), StatusCode::OK);
        let mut body = input();
        body["response_format"] = format.into();
        body["speed"] = 0.7.into();
        let (status, headers, audio) = request(&gateway, body).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["content-type"], mime);
        assert_eq!(audio, "audio-bytes");
        let seen = seen.lock().expect("calls");
        assert_eq!(seen[0]["audio_format"], soniox);
        assert_eq!(seen[0]["speed"], 0.7);
        assert_eq!(
            seen[0].get("sample_rate"),
            (format == "pcm").then_some(&json!(24000))
        );
    }
}

#[tokio::test]
async fn sse_is_client_framing_and_not_a_soniox_parameter() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let gateway = gateway(seen.clone(), StatusCode::OK);
    let mut body = input();
    body["stream_format"] = "sse".into();
    let (status, headers, answer) = request(&gateway, body).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "text/event-stream");
    let answer = std::str::from_utf8(&answer).expect("SSE text");
    assert!(answer.contains("speech.audio.delta"));
    assert!(answer.contains("speech.audio.done"));
    assert!(answer.contains("YXVkaW8t"));
    assert!(
        seen.lock().expect("calls")[0]
            .get("stream_format")
            .is_none()
    );
}

#[tokio::test]
async fn invalid_or_unsupported_fields_fail_before_soniox_is_called() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let gateway = gateway(seen.clone(), StatusCode::OK);
    let mut cases = Vec::new();
    let mut missing = input();
    missing.as_object_mut().expect("input").remove("language");
    cases.push(missing);
    for (key, value) in [
        ("language", json!(123)),
        ("language", json!("")),
        ("voice", json!({"id":"voice"})),
        ("input", json!("")),
        ("instructions", json!("whisper")),
        ("speed", json!(0.6)),
        ("speed", json!(1.4)),
        ("response_format", json!("flac")),
        ("sample_rate", json!(48000)),
        ("unknown", json!(true)),
    ] {
        let mut body = input();
        body[key] = value;
        cases.push(body);
    }
    for body in cases {
        let (status, _, _) = request(&gateway, body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }
    assert!(seen.lock().expect("calls").is_empty());
}

#[tokio::test]
async fn provider_rate_limits_keep_status_code_and_retry_after() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let gateway = gateway(seen, StatusCode::TOO_MANY_REQUESTS);
    let (status, headers, body) = request(&gateway, input()).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(headers["retry-after"], "7");
    let body: Value = serde_json::from_slice(&body).expect("error JSON");
    assert_eq!(body["error"]["code"], "limit_exceeded");
}
