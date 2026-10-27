use super::*;
use respan_gateway_conversation::Message;
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::{Gateway, Provider};
use respan_gateway_http::{
    Body,
    http::{Method, Request},
    http_body_util::BodyExt,
};
use respan_gateway_http_transport::HttpTransport;

fn input(text: &str) -> Input {
    Input::new([Message::new(Role::User, text)])
}
fn scans(guard: &Ark) -> usize {
    guard.0.scans.load(Ordering::Relaxed)
}

#[derive(Debug)]
struct RepeatedCalls;
#[async_trait]
impl Plugin for RepeatedCalls {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>, Fault> {
        let original = cx.input::<Input>()?.clone();
        let call = cx.call("local");
        let mut last = None;
        for step in 0..4 {
            let mut next = original.clone();
            if step >= 2 {
                // A valid tool round-trip, with the same bytes in a new source.
                next.messages.push(Message::new(
                    Role::Assistant,
                    Content::tool_call("tool-1", "lookup", "{}"),
                ));
                next.messages.push(Message::new(
                    Role::User,
                    Content::tool_result(
                        "tool-1",
                        vec![Content::from("Ordinary meeting agenda.")],
                        false,
                    ),
                ));
            }
            if step == 3 {
                next.messages[0].content = vec![Content::from("Updated meeting agenda.")];
            }
            let mut response = call.clone().with_input(next).await?;
            response.until_end().await?;
            last = Some(response);
        }
        Ok(last)
    }
}

#[tokio::test]
async fn request_extensions_reuse_history_but_scan_changed_fields_and_new_tool_results() {
    assert_request_reuse(Config::default()).await;
}
#[cfg(feature = "wolf")]
pub(super) struct WolfTestBundle(pub(super) std::path::PathBuf);
#[cfg(feature = "wolf")]
impl WolfTestBundle {
    pub(super) fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "respan-wolf-tests-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(
            dir.join("model_fp16.onnx"),
            include_bytes!("../../tests/fixtures/wolf_synthetic_fp16.onnx"),
        )
        .unwrap();
        std::fs::write(
            dir.join("tokenizer.mmbpe"),
            include_bytes!("../../tests/fixtures/tokenizer.mmbpe"),
        )
        .unwrap();
        std::fs::write(dir.join("config.json"), r#"{"num_labels":1}"#).unwrap();
        std::fs::write(dir.join("tokenizer_config.json"), "{}").unwrap();
        Self(dir)
    }
}
#[cfg(feature = "wolf")]
impl Drop for WolfTestBundle {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[cfg(feature = "wolf")]
#[tokio::test]
async fn wolf_defender_runs_only_for_new_request_fields() {
    let bundle = WolfTestBundle::new();
    let mut config = Config::default();
    config.scanner.wolf = Some(patronus_ark_core::wolf::WolfConfig::new(&bundle.0, 1.0));
    assert_request_reuse(config).await;
}
async fn assert_request_reuse(config: Config) {
    let fake = mockllm::FakeOpenAi::start().await;
    for _ in 0..8 {
        fake.answer(mockllm::Answer::text("ok"));
    }
    let guard = Ark::new(config).unwrap();
    let mut headers = respan_gateway_http::http::HeaderMap::new();
    headers.insert(
        "authorization",
        respan_gateway_http::http::HeaderValue::from_static("Bearer sk-test"),
    );
    let transport = HttpTransport::new(fake.base_url())
        .unwrap()
        .with_headers(headers)
        .unwrap();
    let gateway = Gateway::new()
        .provider(Provider::new("local", OpenAiChat, transport.connect()))
        .unwrap()
        .client::<Input, _, _>(OpenAiChat)
        .unwrap()
        .plugin(guard.clone())
        .plugin(RepeatedCalls)
        .plugin(guard.provider_guard());
    for request_index in 1..=2 {
        let mut request = Request::new(Body::full(
            serde_json::json!({
                "model":"test", "messages":[{"role":"user","content":"Ordinary meeting agenda."}]
            })
            .to_string(),
        ));
        *request.method_mut() = Method::POST;
        *request.uri_mut() = "/v1/chat/completions".parse().unwrap();
        let (response, ended) = respan_gateway_http::handle(&gateway, request, 1 << 20).await;
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let _ = ended.await;
        // User + tool arguments + tool result + changed user; history is reused.
        assert_eq!(scans(&guard), request_index * 4);
        #[cfg(feature = "wolf")]
        if guard.0.config.scanner.wolf.is_some() {
            assert_eq!(
                guard.0.model_documents.load(Ordering::Relaxed),
                request_index * 3
            );
        }
    }
    assert_eq!(fake.requests().len(), 8);
}

#[tokio::test]
async fn released_masked_fields_are_reused_and_policy_instances_are_isolated() {
    let guard = Ark::new(Config {
        pii: Action::Mask,
        ..Default::default()
    })
    .unwrap();
    let ledger = RequestChecks::default();
    let sanitized = guard
        .inspect_request(input("alice@example.org"), ledger.clone())
        .await
        .unwrap();
    assert_eq!(sanitized.messages[0].text(), "[REDACTED]");
    assert_eq!(scans(&guard), 1);
    guard
        .inspect_request(sanitized.clone(), ledger.clone())
        .await
        .unwrap();
    assert_eq!(scans(&guard), 1);
    let other = Ark::new(Config::default()).unwrap();
    other.inspect_request(sanitized, ledger).await.unwrap();
    assert_eq!(scans(&other), 1);
}

#[tokio::test]
async fn parallel_call_rechecks_after_acquiring_active_slot() {
    let guard = Ark::new(Config {
        queue_timeout_ms: 60_000,
        ..Default::default()
    })
    .unwrap();
    let ledger = RequestChecks::default();
    let held = Arc::clone(&guard.0.active).acquire_owned().await.unwrap();
    let first = {
        let guard = guard.clone();
        let ledger = ledger.clone();
        tokio::spawn(async move {
            guard
                .inspect_request(input("Ordinary meeting agenda."), ledger)
                .await
        })
    };
    let second = {
        let guard = guard.clone();
        let ledger = ledger.clone();
        tokio::spawn(async move {
            guard
                .inspect_request(input("Ordinary meeting agenda."), ledger)
                .await
        })
    };
    tokio::task::yield_now().await;
    drop(held);
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    assert_eq!(scans(&guard), 1);
}

#[tokio::test]
async fn full_ledger_rescans_and_failed_inspection_publishes_no_stamps() {
    let guard = Ark::new(Config::default()).unwrap();
    let ledger = RequestChecks::default();
    for index in 0..MAX_REQUEST_STAMPS {
        ledger.0.lock().unwrap().insert(ScanStamp::new(
            guard.0.identity,
            ScanSource::UserInput,
            ScanChecks::default(),
            0,
            &format!("stamp-{index}"),
        ));
    }
    for _ in 0..2 {
        guard
            .inspect_request(input("Ordinary meeting agenda."), ledger.clone())
            .await
            .unwrap();
    }
    assert_eq!(scans(&guard), 2);
    assert_eq!(ledger.0.lock().unwrap().len(), MAX_REQUEST_STAMPS);
    let empty = RequestChecks::default();
    let mut chat = input("Ordinary meeting agenda.");
    chat.messages[0]
        .content
        .push(Content::tool_call("call", "tool", "invalid-json"));
    assert!(guard.inspect_request(chat, empty.clone()).await.is_err());
    assert!(empty.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn cancellation_stops_work_before_scanning_and_holds_capacity_until_job_returns() {
    let guard = Ark::new(Config {
        max_queued: 0,
        ..Default::default()
    })
    .unwrap();
    let ledger = RequestChecks::default();
    let state = Arc::clone(&guard.0);
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let blocker = std::thread::spawn(move || {
        let _lock = state.scanner.lock().unwrap();
        held_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    held_rx.recv().unwrap();
    let task = {
        let guard = guard.clone();
        let ledger = ledger.clone();
        tokio::spawn(async move {
            guard
                .inspect_request(input("Ordinary meeting agenda."), ledger)
                .await
        })
    };
    while guard.0.active.available_permits() != 0 {
        tokio::task::yield_now().await;
    }
    task.abort();
    let _ = task.await;
    assert_eq!(guard.0.active.available_permits(), 0);
    release_tx.send(()).unwrap();
    blocker.join().unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while guard.0.active.available_permits() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(scans(&guard), 0);
    assert!(ledger.0.lock().unwrap().is_empty());
    guard
        .inspect_request(input("Ordinary meeting agenda."), ledger)
        .await
        .unwrap();
    assert_eq!(scans(&guard), 1);
}

#[test]
fn core_errors_keep_their_categories_and_http_statuses() {
    for (error, code, status) in [
        (
            ScanError::InputLimit { limit: 1 },
            "ark_input_limit",
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
        (
            ScanError::FindingLimit { limit: 1 },
            "ark_finding_limit",
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
        (
            ScanError::DeadlineExceeded,
            "ark_scan_timeout",
            StatusCode::GATEWAY_TIMEOUT,
        ),
    ] {
        let fault = scan_fault(error);
        assert_eq!(fault.code(), code);
        assert_eq!(fault.get::<Status>().unwrap().0, status);
    }
    for (error, code) in [
        (
            ScanError::Asset("private diagnostic".into()),
            "ark_asset_failed",
        ),
        (
            ScanError::Inference("private diagnostic".into()),
            "ark_inference_failed",
        ),
        (
            ScanError::Detector("private diagnostic".into()),
            "ark_detector_failed",
        ),
    ] {
        let fault = scan_fault(error);
        assert_eq!(fault.code(), code);
        assert_eq!(fault.kind(), respan_gateway_core::FaultKind::Gateway);
        assert!(!fault.message().contains("private diagnostic"));
    }
    assert_eq!(
        scan_fault(ScanError::Cancelled).kind(),
        respan_gateway_core::FaultKind::Abandoned
    );
    assert_eq!(
        fault("ark_overloaded", "busy").get::<Status>().unwrap().0,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        fault("ark_queue_timeout", "busy")
            .get::<Status>()
            .unwrap()
            .0,
        StatusCode::GATEWAY_TIMEOUT
    );
}

#[tokio::test]
async fn tool_argument_injection_is_one_decoded_document_not_each_scalar() {
    let guard = Ark::new(Config {
        pii_sources: vec![],
        dlp_sources: vec![],
        pi_sources: vec![ScanSource::ToolCall],
        ..Default::default()
    })
    .unwrap();
    let mut chat = input("");
    chat.messages[0].content.push(Content::tool_call(
        "call",
        "tool",
        r#"{"a":"first","b":["second","third"],"c":42}"#,
    ));
    let ledger = RequestChecks::default();
    let checked = guard.inspect_request(chat, ledger.clone()).await.unwrap();
    assert_eq!(scans(&guard), 1);
    guard.inspect_request(checked, ledger).await.unwrap();
    assert_eq!(scans(&guard), 1);
    let document = decoded_json_document(
        &serde_json::from_str(r#"{"k":"hel\u006co"}"#).unwrap(),
        &ScanControl::default(),
    )
    .unwrap();
    assert!(document.contains("hello"));
    assert!(!document.contains("\\u006c"));
}
