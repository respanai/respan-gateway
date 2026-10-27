use super::*;
use respan_gateway_conversation::{Message, Role};
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::{Gateway, Provider};
use respan_gateway_http::{
    Body,
    http::{Method, Request},
    http_body_util::BodyExt,
};
use respan_gateway_http_transport::HttpTransport;

fn test_transport(url: &str) -> HttpTransport {
    let mut headers = respan_gateway_http::http::HeaderMap::new();
    headers.insert(
        "authorization",
        respan_gateway_http::http::HeaderValue::from_static("Bearer sk-test"),
    );
    HttpTransport::new(url)
        .unwrap()
        .with_headers(headers)
        .unwrap()
}
fn input(text: &str) -> Input {
    Input::new([Message::new(Role::User, text)])
}
fn config() -> Config {
    Config {
        pii: Action::Mask,
        ..Default::default()
    }
}
#[test]
fn masks_original_utf8_spans_and_keeps_json_valid() {
    let config = config();
    let mut scanner = Scanner::new(config.scanner.clone()).unwrap();
    let mut chat = input("Grüße: alice@example.org");
    chat.messages[0].content.push(Content::tool_call(
        "1",
        "mail",
        r#"{"to":"alice\u0040example.org","nested":["bob@example.net"]}"#,
    ));
    let checked = inspect_input(chat, &mut scanner, &config).unwrap();
    assert_eq!(checked.messages[0].text(), "Grüße: [REDACTED]");
    if let Content::ToolCall { arguments, .. } = &checked.messages[0].content[1] {
        let value: serde_json::Value = serde_json::from_str(arguments).unwrap();
        assert_eq!(value["to"], "[REDACTED]");
        assert_eq!(value["nested"][0], "[REDACTED]");
    } else {
        panic!("missing tool call");
    }
}
#[test]
fn nested_tool_results_are_scanned_and_limits_fail_closed() {
    let config = Config::default();
    let mut scanner = Scanner::new(config.scanner.clone()).unwrap();
    let mut chat = input("hello");
    chat.messages[0].content = vec![Content::tool_result(
        "1",
        vec![Content::tool_result(
            "2",
            vec![Content::from("alice@example.org")],
            false,
        )],
        false,
    )];
    assert!(inspect_input(chat, &mut scanner, &config).is_err());
    assert!(
        preflight(
            &input("too long"),
            &Config {
                max_request_bytes: 2,
                ..config
            }
        )
        .is_err()
    );
}
async fn send(gateway: &Gateway, text: &str) -> u16 {
    let mut request = Request::new(Body::full(
        serde_json::json!({"model":"test","messages":[{"role":"user","content":text}]}).to_string(),
    ));
    *request.method_mut() = Method::POST;
    *request.uri_mut() = "/v1/chat/completions".parse().unwrap();
    let (response, ended) = respan_gateway_http::handle(gateway, request, 1 << 20).await;
    let status = response.status().as_u16();
    let _ = response.into_body().collect().await;
    let _ = ended.await;
    status
}
#[tokio::test]
async fn blocked_input_never_reaches_provider() {
    let fake = mockllm::FakeOpenAi::start().await;
    let guard = Ark::new(Config::default()).unwrap();
    let gateway = Gateway::new()
        .provider(Provider::new(
            "local",
            OpenAiChat,
            test_transport(&fake.base_url()).connect(),
        ))
        .unwrap()
        .client::<Input, _, _>(OpenAiChat)
        .unwrap()
        .plugin(guard.clone())
        .plugin(guard.provider_guard())
        .target("local")
        .unwrap();
    assert_eq!(send(&gateway, "alice@example.org").await, 400);
    assert!(fake.requests().is_empty());
}
#[derive(Debug)]
struct AddSensitiveProviderInput;
#[async_trait]
impl Plugin for AddSensitiveProviderInput {
    async fn on_provider(&self, px: &mut ProviderContext) -> Result<(), Fault> {
        px.filter(|input: &mut Input| {
            input
                .messages
                .push(Message::new(Role::User, "alice@example.org"));
            Ok(())
        })
    }
}
#[tokio::test]
async fn provider_guard_catches_content_added_after_client_guard() {
    let fake = mockllm::FakeOpenAi::start().await;
    let guard = Ark::new(Config::default()).unwrap();
    let gateway = Gateway::new()
        .provider(Provider::new(
            "local",
            OpenAiChat,
            test_transport(&fake.base_url()).connect(),
        ))
        .unwrap()
        .client::<Input, _, _>(OpenAiChat)
        .unwrap()
        .plugin(guard.clone())
        .plugin(AddSensitiveProviderInput)
        .plugin(guard.provider_guard())
        .target("local")
        .unwrap();
    assert_eq!(send(&gateway, "hello").await, 400);
    assert!(fake.requests().is_empty());
}
#[tokio::test]
async fn masked_request_reaches_provider_without_sensitive_value() {
    let fake = mockllm::FakeOpenAi::start().await;
    fake.answer(mockllm::Answer::text("ok"));
    let guard = Ark::new(config()).unwrap();
    let gateway = Gateway::new()
        .provider(Provider::new(
            "local",
            OpenAiChat,
            test_transport(&fake.base_url()).connect(),
        ))
        .unwrap()
        .client::<Input, _, _>(OpenAiChat)
        .unwrap()
        .plugin(guard.clone())
        .plugin(guard.provider_guard())
        .target("local")
        .unwrap();
    assert_eq!(send(&gateway, "alice@example.org").await, 200);
    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    let body = requests[0].body.to_string();
    assert!(!body.contains("alice@example.org"));
    assert!(body.contains("[REDACTED]"));
}
#[tokio::test]
async fn timed_out_work_keeps_capacity_until_the_blocking_job_ends() {
    let guard = Ark::new(Config {
        max_queued: 0,
        scan_timeout_ms: 20,
        ..config()
    })
    .unwrap();
    let state = Arc::clone(&guard.0);
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let blocker = std::thread::spawn(move || {
        let _lock = state.scanner.lock().unwrap();
        held_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    held_rx.recv().unwrap();
    assert_eq!(
        guard.inspect(input("hello")).await.unwrap_err().code(),
        "ark_scan_timeout"
    );
    assert_eq!(guard.0.active.available_permits(), 0);
    assert_eq!(guard.0.slots.available_permits(), 0);
    assert_eq!(
        guard.inspect(input("hello")).await.unwrap_err().code(),
        "ark_overloaded"
    );
    release_tx.send(()).unwrap();
    blocker.join().unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while guard.0.slots.available_permits() == 0 || guard.0.active.available_permits() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(guard.0.active.available_permits(), 1);
    assert_eq!(guard.0.slots.available_permits(), 1);
}
