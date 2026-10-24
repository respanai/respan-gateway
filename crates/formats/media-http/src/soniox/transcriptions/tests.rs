use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::openai::OpenAiTranscriptions;
use crate::soniox::SonioxTranscriptions;
use respan_gateway_core::{Gateway, Plugin, ProviderContext, Result, async_trait};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::call::{CallHeaders, prepare_call};
use respan_gateway_http::http::{self, Method, StatusCode};
use respan_gateway_http::{Body, Connect, Upstream};
use respan_gateway_media::transcription::{Input, Output};
use serde_json::{Value, json};

const FILE: &str = "11111111-1111-4111-8111-111111111111";
const JOB: &str = "22222222-2222-4222-8222-222222222222";

#[derive(Clone)]
struct Step {
    method: Method,
    path: String,
    status: u16,
    answer: Value,
    hang: bool,
}

fn step(method: Method, path: impl Into<String>, status: u16, answer: Value) -> Step {
    Step {
        method,
        path: path.into(),
        status,
        answer,
        hang: false,
    }
}

fn success_steps(text: &str) -> Vec<Step> {
    vec![
        step(Method::POST, "/files", 201, json!({"id": FILE})),
        step(
            Method::POST,
            "/transcriptions",
            201,
            json!({"id": JOB, "status": "queued"}),
        ),
        step(
            Method::GET,
            format!("/transcriptions/{JOB}"),
            200,
            json!({"id": JOB, "status": "processing"}),
        ),
        step(
            Method::GET,
            format!("/transcriptions/{JOB}"),
            200,
            json!({"id": JOB, "status": "completed", "audio_duration_ms": 1250}),
        ),
        step(
            Method::GET,
            format!("/transcriptions/{JOB}/transcript"),
            200,
            json!({"id": JOB, "text": text, "tokens": []}),
        ),
        step(
            Method::DELETE,
            format!("/transcriptions/{JOB}"),
            204,
            Value::Null,
        ),
        step(Method::DELETE, format!("/files/{FILE}"), 204, Value::Null),
    ]
}

#[derive(Clone, Default)]
struct Fake {
    steps: Arc<Mutex<VecDeque<Step>>>,
    seen: Arc<Mutex<Vec<(Method, String, Bytes)>>>,
}

impl Fake {
    fn new(steps: Vec<Step>) -> Self {
        Self {
            steps: Arc::new(Mutex::new(steps.into())),
            ..Self::default()
        }
    }

    fn gateway(&self) -> Gateway {
        let fake = self.clone();
        let send = move |mut request: http::Request<Body>| {
            let fake = fake.clone();
            async move {
                // Run the real transport's credential/base preparation so
                // assertions cover the outgoing header, including cleanup.
                prepare_call(
                    &mut request,
                    "https://api.soniox.test/v1",
                    &http::HeaderMap::new(),
                )?;
                assert_eq!(request.headers()["authorization"], "Bearer test-soniox-key");
                let (head, body) = request.into_parts();
                let body = body.read_answer(1 << 20).await?;
                fake.seen.lock().expect("seen").push((
                    head.method.clone(),
                    head.uri.to_string(),
                    body,
                ));
                let step = fake
                    .steps
                    .lock()
                    .expect("steps")
                    .pop_front()
                    .expect("unexpected provider call");
                assert_eq!(head.method, step.method);
                assert_eq!(head.uri.path(), format!("/v1{}", step.path));
                if step.hang {
                    futures::future::pending::<()>().await;
                }
                let body = if step.status == 204 {
                    Bytes::new()
                } else {
                    Bytes::from(step.answer.to_string())
                };
                let answer = http::Response::builder()
                    .status(step.status)
                    .header("content-type", "application/json")
                    .body(Body::full(body))
                    .expect("fake response");
                Ok((answer, None))
            }
        };
        let factory = respan_gateway_http::connect(send, 1 << 20);
        let connect: Connect = Arc::new(factory);
        Gateway::new()
            .provider(SonioxTranscriptions.provider("soniox".into(), connect))
            .expect("provider")
            .client::<Input, Output, _>(OpenAiTranscriptions)
            .expect("client")
            .plugin(Byok)
            .target("soniox")
            .expect("target")
    }

    fn finished(&self) {
        assert!(
            self.steps.lock().expect("steps").is_empty(),
            "expected provider calls remain"
        );
    }
}

struct Byok;

#[async_trait]
impl Plugin for Byok {
    async fn on_provider(&self, px: &mut ProviderContext) -> Result<()> {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            "authorization",
            "Bearer test-soniox-key".parse().expect("header"),
        );
        px.extensions_mut().insert(CallHeaders(headers));
        Ok(())
    }
}

fn request(fields: &[(&str, &str)], path: &str) -> http::Request<Body> {
    let mut body = String::from(
        "--test\r\nContent-Disposition: form-data; name=\"file\"; filename=\"speech.wav\"\r\nContent-Type: audio/wav\r\n\r\naudio-fixture\r\n",
    );
    for (name, value) in std::iter::once(&("model", "stt-async-v5")).chain(fields.iter()) {
        body.push_str(&format!(
            "--test\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
        ));
    }
    body.push_str("--test--\r\n");
    http::Request::builder()
        .method("POST")
        .uri(path)
        .header("authorization", "Bearer gateway-client-key")
        .header("content-type", "multipart/form-data; boundary=test")
        .body(Body::full(body))
        .expect("request")
}

async fn send(gateway: &Gateway, fields: &[(&str, &str)]) -> (StatusCode, Bytes) {
    send_to(gateway, fields, "/v1/audio/transcriptions").await
}

async fn send_to(gateway: &Gateway, fields: &[(&str, &str)], path: &str) -> (StatusCode, Bytes) {
    let (answer, _) = respan_gateway_http::handle(gateway, request(fields, path), 1 << 20).await;
    let status = answer.status();
    let body = answer
        .into_body()
        .read_answer(1 << 20)
        .await
        .expect("client body");
    gateway.idle().await;
    (status, body)
}

#[tokio::test(start_paused = true)]
async fn upload_job_poll_transcript_and_cleanup_are_one_openai_response() {
    let fake = Fake::new(success_steps("Hello world"));
    let (status, body) = send(
        &fake.gateway(),
        &[("language", "en"), ("prompt", "product names")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        serde_json::from_slice::<Value>(&body).expect("json"),
        json!({"text": "Hello world", "duration": 1.25, "usage": {"type": "duration", "seconds": 1.25}})
    );
    let seen = fake.seen.lock().expect("seen");
    assert!(String::from_utf8_lossy(&seen[0].2).contains("audio-fixture"));
    assert_eq!(
        serde_json::from_slice::<Value>(&seen[1].2).expect("create"),
        json!({"model": "stt-async-v5", "file_id": FILE, "language_hints": ["en"], "context": "product names"})
    );
    drop(seen);
    fake.finished();
}

#[tokio::test(start_paused = true)]
async fn plain_text_and_silence_are_valid_transcripts() {
    for text in ["Hello world", ""] {
        let fake = Fake::new(success_steps(text));
        let (status, body) = send(&fake.gateway(), &[("response_format", "text")]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_ref(), text.as_bytes());
        fake.finished();
    }
}

#[tokio::test]
async fn unsupported_requests_fail_before_any_upload() {
    for fields in [
        vec![("stream", "true")],
        vec![("temperature", "0")],
        vec![("response_format", "verbose_json")],
        vec![("timestamp_granularities[]", "word")],
        vec![("unexpected", "value")],
    ] {
        for path in ["/v1/audio/transcriptions", "/v1/audio/translations"] {
            let fake = Fake::default();
            let (status, _) = send_to(&fake.gateway(), &fields, path).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{fields:?}");
            assert!(fake.seen.lock().expect("seen").is_empty());
        }
    }
}

#[tokio::test]
async fn upload_refusal_and_malformed_file_id_never_retry_or_create_a_job() {
    for (status, answer, expected) in [
        (
            429,
            json!({"message": "rate limited", "error_type": "limit_exceeded"}),
            StatusCode::TOO_MANY_REQUESTS,
        ),
        (
            503,
            json!({"message": "overloaded"}),
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (201, json!({"id": "../not-a-uuid"}), StatusCode::BAD_GATEWAY),
    ] {
        let fake = Fake::new(vec![step(Method::POST, "/files", status, answer)]);
        let (status, _) = send(&fake.gateway(), &[]).await;
        assert_eq!(status, expected);
        fake.finished();
        assert_eq!(fake.seen.lock().expect("seen").len(), 1);
    }
}

#[tokio::test]
async fn failed_create_cleans_only_the_owned_file_and_never_retries() {
    let fake = Fake::new(vec![
        step(Method::POST, "/files", 201, json!({"id": FILE})),
        step(
            Method::POST,
            "/transcriptions",
            500,
            json!({"message": "failed"}),
        ),
        step(Method::DELETE, format!("/files/{FILE}"), 204, Value::Null),
    ]);
    let (status, _) = send(&fake.gateway(), &[]).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    fake.finished();
}

#[tokio::test(start_paused = true)]
async fn terminal_job_error_cleans_job_and_file() {
    for path in ["/v1/audio/transcriptions", "/v1/audio/translations"] {
        let mut steps = success_steps("unused");
        steps[2].answer = json!({"status": "error", "error_type": "audio_decode_failed"});
        steps.drain(3..5);
        let fake = Fake::new(steps);
        let (status, body) = send_to(&fake.gateway(), &[], path).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(String::from_utf8_lossy(&body).contains("audio_decode_failed"));
        fake.finished();
    }
}

#[tokio::test(start_paused = true)]
async fn invalid_transcript_and_duration_are_provider_errors_with_cleanup() {
    for malformed in [json!({"id": JOB}), json!({"id": JOB, "text": 123})] {
        let mut steps = success_steps("unused");
        steps[4].answer = malformed;
        let fake = Fake::new(steps);
        let (status, _) = send(&fake.gateway(), &[]).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        fake.finished();
    }
    let mut steps = success_steps("unused");
    steps[3].answer = json!({"status": "completed", "audio_duration_ms": -1});
    let fake = Fake::new(steps);
    let (status, _) = send(&fake.gateway(), &[]).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    fake.finished();
}

#[tokio::test(start_paused = true)]
async fn cleanup_failure_preserves_success_and_missing_resources_are_already_clean() {
    for status in [404, 500] {
        let mut steps = success_steps("Hello");
        steps[5].status = status;
        steps[5].answer = json!({"message": "cleanup refused"});
        if status == 500 {
            steps.pop();
        }
        let fake = Fake::new(steps);
        let (status, body) = send(&fake.gateway(), &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert!(String::from_utf8_lossy(&body).contains("Hello"));
        fake.finished();
    }
}

#[tokio::test(start_paused = true)]
async fn cleanup_conflict_waits_for_terminal_state_before_deleting_the_file() {
    let mut steps = success_steps("Hello");
    steps[5].status = 409;
    steps[5].answer = json!({"message": "processing"});
    steps.insert(
        6,
        step(
            Method::GET,
            format!("/transcriptions/{JOB}"),
            200,
            json!({"status": "completed"}),
        ),
    );
    steps.insert(
        7,
        step(
            Method::DELETE,
            format!("/transcriptions/{JOB}"),
            204,
            Value::Null,
        ),
    );
    let fake = Fake::new(steps);
    let (status, _) = send(&fake.gateway(), &[]).await;
    assert_eq!(status, StatusCode::OK);
    fake.finished();
}

async fn wait_for_calls(fake: &Fake, count: usize) {
    for _ in 0..1000 {
        if fake.seen.lock().expect("seen").len() >= count {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("provider did not receive {count} calls");
}

#[tokio::test(start_paused = true)]
async fn cancellation_at_each_boundary_runs_only_owned_resource_cleanup() {
    for interrupted in 0..4 {
        let mut steps = success_steps("unused");
        // Use one completed poll so the fourth boundary is transcript retrieval.
        steps.remove(2);
        steps[interrupted].hang = true;
        steps.truncate(interrupted + 1);
        if interrupted >= 2 {
            if interrupted == 2 {
                steps.push(step(
                    Method::GET,
                    format!("/transcriptions/{JOB}"),
                    200,
                    json!({"status": "completed"}),
                ));
            }
            steps.push(step(
                Method::DELETE,
                format!("/transcriptions/{JOB}"),
                204,
                Value::Null,
            ));
        }
        if interrupted >= 1 {
            steps.push(step(
                Method::DELETE,
                format!("/files/{FILE}"),
                204,
                Value::Null,
            ));
        }
        let fake = Fake::new(steps);
        let gateway = fake.gateway();
        let running = gateway.clone();
        let task = tokio::spawn(async move { send(&running, &[]).await });
        if interrupted >= 2 {
            wait_for_calls(&fake, 2).await;
            tokio::time::advance(std::time::Duration::from_secs(1)).await;
        }
        wait_for_calls(&fake, interrupted + 1).await;
        task.abort();
        assert!(task.await.expect_err("aborted request").is_cancelled());
        gateway.idle().await;
        fake.finished();
    }
}

#[tokio::test(start_paused = true)]
async fn overall_deadline_and_processing_cleanup_are_bounded_without_duplicate_jobs() {
    let mut steps = success_steps("unused");
    steps.truncate(3);
    steps[2].hang = true;
    let mut cleanup = step(
        Method::GET,
        format!("/transcriptions/{JOB}"),
        200,
        Value::Null,
    );
    cleanup.hang = true;
    steps.push(cleanup);
    let fake = Fake::new(steps);
    let gateway = fake.gateway();
    let task = tokio::spawn(async move { send(&gateway, &[]).await });
    wait_for_calls(&fake, 2).await;
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    wait_for_calls(&fake, 3).await;
    tokio::time::advance(std::time::Duration::from_secs(600)).await;
    wait_for_calls(&fake, 4).await;
    tokio::time::advance(std::time::Duration::from_secs(10)).await;
    let (status, _) = task.await.expect("deadline response");
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
    fake.finished();
    let seen = fake.seen.lock().expect("seen");
    assert_eq!(
        seen.iter()
            .filter(|(method, _, _)| method == Method::POST)
            .count(),
        2
    );
    assert!(seen.iter().all(|(method, _, _)| method != Method::DELETE));
}

#[tokio::test]
async fn real_http_transport_authenticates_every_internal_request_over_a_socket() {
    socket_response(false).await;
    socket_response(true).await;
}

async fn socket_response(translate: bool) {
    let mut steps = success_steps("Socket transcript");
    steps.remove(2);
    if translate {
        steps[3].answer = translation_transcript();
    }
    let fake = Fake::new(steps);
    let upstream = fake.clone();
    let router = axum::Router::new().fallback(move |request: axum::extract::Request| {
        let upstream = upstream.clone();
        async move {
            assert_eq!(request.headers()["authorization"], "Bearer test-soniox-key");
            let (head, body) = request.into_parts();
            let body = axum::body::to_bytes(body, 1 << 20).await.expect("upload");
            upstream.seen.lock().expect("seen").push((
                head.method.clone(),
                head.uri.to_string(),
                body,
            ));
            let step = upstream
                .steps
                .lock()
                .expect("steps")
                .pop_front()
                .expect("provider call");
            assert_eq!(head.method, step.method);
            assert_eq!(head.uri.path(), format!("/v1{}", step.path));
            let body = if step.status == 204 {
                String::new()
            } else {
                step.answer.to_string()
            };
            http::Response::builder()
                .status(step.status)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body))
                .expect("response")
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let address = listener.local_addr().expect("local address");
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    let transport =
        respan_gateway_http_transport::HttpTransport::new(format!("http://{address}/v1"))
            .expect("transport")
            .with_network(&respan_gateway_http_transport::Network::default().direct())
            .expect("local network");
    let gateway = Gateway::new()
        .provider(SonioxTranscriptions.provider("soniox".into(), Arc::new(transport.connect())))
        .expect("provider")
        .client::<Input, Output, _>(OpenAiTranscriptions)
        .expect("client")
        .plugin(Byok)
        .target("soniox")
        .expect("target");
    let path = if translate {
        "/v1/audio/translations"
    } else {
        "/v1/audio/transcriptions"
    };
    let (status, body) = send_to(&gateway, &[], path).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        serde_json::from_slice::<Value>(&body).expect("json")["text"],
        if translate {
            "Hello world. Already English."
        } else {
            "Socket transcript"
        }
    );
    fake.finished();
    assert_eq!(fake.seen.lock().expect("seen").len(), 6);
    server.abort();
}

fn translation_transcript() -> Value {
    json!({"text": "Hola mundo.Hello world. Already English.", "tokens": [
        {"text": "Hola mundo.", "translation_status": "original", "language": "es"},
        {"text": "Hello", "translation_status": "translation", "language": "en"},
        {"text": " world.", "translation_status": "translation", "language": "en"},
        {"text": "bonjour", "translation_status": "none", "language": "fr"},
        {"text": " Already English.", "translation_status": "none", "language": "en"}
    ]})
}

#[tokio::test(start_paused = true)]
async fn translation_returns_only_english_in_order_as_json_or_text_with_duration_and_cleanup() {
    for format in ["json", "text"] {
        let mut steps = success_steps("unused mixed transcript");
        steps[4].answer = translation_transcript();
        let fake = Fake::new(steps);
        let (status, body) = send_to(
            &fake.gateway(),
            &[("response_format", format)],
            "/v1/audio/translations",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        if format == "json" {
            assert_eq!(
                serde_json::from_slice::<Value>(&body).expect("json"),
                json!({
                    "text": "Hello world. Already English.", "duration": 1.25,
                    "usage": {"type": "duration", "seconds": 1.25}
                })
            );
        } else {
            assert_eq!(body.as_ref(), b"Hello world. Already English.");
        }
        let seen = fake.seen.lock().expect("seen");
        assert_eq!(
            serde_json::from_slice::<Value>(&seen[1].2).expect("create"),
            json!({
                "model": "stt-async-v5", "file_id": FILE,
                "translation": {"type": "one_way", "target_language": "en"}
            })
        );
        drop(seen);
        fake.finished();
    }
}

#[tokio::test(start_paused = true)]
async fn translation_silence_is_valid_but_missing_or_invalid_english_tokens_fail_with_cleanup() {
    for (transcript, expected) in [
        (json!({"tokens": []}), StatusCode::OK),
        (
            json!({"text": "must not fall back"}),
            StatusCode::BAD_GATEWAY,
        ),
        (
            json!({"tokens": [{"text": "Hola", "translation_status": "original", "language": "es"}]}),
            StatusCode::BAD_GATEWAY,
        ),
        (
            json!({"tokens": [{"text": 123, "translation_status": "translation"}]}),
            StatusCode::BAD_GATEWAY,
        ),
    ] {
        let mut steps = success_steps("unused");
        steps[4].answer = transcript;
        let fake = Fake::new(steps);
        let (status, _) = send_to(&fake.gateway(), &[], "/v1/audio/translations").await;
        assert_eq!(status, expected);
        fake.finished();
    }
}
