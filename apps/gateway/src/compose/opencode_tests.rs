use super::{
    FakeAnthropic, FakeOpenAi, Gateway, StatusCode, http, post, read_whole, resolve, serve,
};
use respan_gateway_http::Body;
use serde_json::json;

const CHAT: &str = r#"{"model":"zen/test","messages":[{"role":"user","content":"hi"}]}"#;
const MESSAGES: &str =
    r#"{"model":"zen/test","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#;

fn chat_gateway(base: &str, vendor: &str) -> Gateway {
    resolve(&format!(
        r#"
[providers.zen]
vendor = "{vendor}"
key = "zen-key"
proxy = false
formats = ["openai-chat"]
base_url = "{base}"
"#,
    ))
    .expect("gateway")
}

async fn send(gateway: &Gateway, req: http::Request<Body>) {
    let (resp, _) = serve(gateway, req).await;
    let (status, _, body) = read_whole(resp).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    gateway.idle().await;
}

#[test]
fn registers_the_three_default_endpoints() {
    let vendor = respan_gateway_vendors::find("opencode").expect("registered vendor");
    let endpoints: Vec<_> = vendor
        .formats
        .iter()
        .map(|(format, base)| (format.name(), *base))
        .collect();
    assert_eq!(
        endpoints,
        [
            ("openai-chat", Some("https://opencode.ai/zen/v1")),
            ("openai-responses", Some("https://opencode.ai/zen/v1")),
            (
                "anthropic-messages",
                Some("https://opencode.ai/inference/anthropic"),
            ),
        ]
    );
}

#[tokio::test]
async fn carries_only_session_headers_on_each_format() {
    let openai = FakeOpenAi::start().await;
    let anthropic = FakeAnthropic::start().await;
    let gateway = resolve(&format!(
        r#"
[providers.zen]
vendor = "opencode"
key = "zen-key"
proxy = false
formats = {{ openai-chat = "{oa}", openai-responses = "{oa}", anthropic-messages = "{ant}" }}
"#,
        oa = openai.base_url(),
        ant = anthropic.base_url(),
    ))
    .expect("gateway");
    let headers = [
        ("x-opencode-session", "session"),
        ("x-session-id", "session-alias"),
        ("x-opencode-session-id", "related-session"),
        ("x-opencode-parent-session-id", "parent"),
        ("x-parent-session-id", "parent-alias"),
        ("x-session-affinity", "affinity"),
        ("x-opencode-project", "project"),
        ("x-opencode-client", "client"),
    ];
    for (path, body) in [
        ("/v1/chat/completions", CHAT),
        ("/v1/responses", r#"{"model":"zen/test","input":"hi"}"#),
        ("/v1/messages", MESSAGES),
    ] {
        let mut req = post(path, body);
        for (name, value) in headers.into_iter().chain([
            ("x-opencode-unlisted", "private"),
            ("authorization", "Bearer client-key"),
        ]) {
            req.headers_mut()
                .insert(name, http::HeaderValue::from_static(value));
        }
        send(&gateway, req).await;
    }

    let oa = openai.requests();
    let ant = anthropic.requests();
    assert_eq!(
        oa.iter().map(|call| call.path.as_str()).collect::<Vec<_>>(),
        ["/v1/chat/completions", "/v1/responses"]
    );
    assert_eq!(
        ant.iter()
            .map(|call| call.path.as_str())
            .collect::<Vec<_>>(),
        ["/v1/messages"]
    );
    for call in oa.iter().chain(&ant) {
        for (name, value) in headers {
            assert_eq!(call.header(name), Some(value), "{} {name}", call.path);
            assert_eq!(
                call.headers.get_all(name).iter().count(),
                1,
                "{} {name}",
                call.path
            );
        }
        assert_eq!(call.header("x-opencode-unlisted"), None);
    }
    assert_eq!(oa[0].header("authorization"), Some("Bearer zen-key"));
    assert_eq!(ant[0].header("x-api-key"), Some("zen-key"));
    assert_eq!(ant[0].header("authorization"), None);
}

#[tokio::test]
async fn keeps_the_session_when_translating_messages_to_chat() {
    let openai = FakeOpenAi::start().await;
    let gateway = chat_gateway(&openai.base_url(), "opencode");
    let mut req = post("/v1/messages", MESSAGES);
    req.headers_mut()
        .insert("x-session-id", http::HeaderValue::from_static("session"));
    send(&gateway, req).await;
    let calls = openai.requests();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].path, "/v1/chat/completions");
    assert_eq!(
        calls[0].body["messages"],
        json!([{"role":"user","content":"hi"}])
    );
    assert_eq!(calls[0].header("x-session-id"), Some("session"));
    assert_eq!(calls[0].header("x-opencode-session"), None);
}

#[tokio::test]
async fn derives_stable_credential_sessions_and_fresh_anonymous_sessions() {
    let openai = FakeOpenAi::start().await;
    let gateway = chat_gateway(&openai.base_url(), "opencode");
    for key in [
        Some("Bearer client-a"),
        Some("Bearer client-a"),
        Some("Bearer client-b"),
        None,
        None,
    ] {
        let mut req = post("/v1/chat/completions", CHAT);
        req.headers_mut().insert(
            "x-opencode-project",
            http::HeaderValue::from_static("project"),
        );
        if let Some(key) = key {
            req.headers_mut()
                .insert("authorization", http::HeaderValue::from_static(key));
        }
        send(&gateway, req).await;
    }
    let calls = openai.requests();
    assert_eq!(calls.len(), 5);
    let sessions: Vec<_> = calls
        .iter()
        .map(|call| {
            assert_eq!(call.header("x-opencode-project"), Some("project"));
            call.header("x-opencode-session").expect("session ID")
        })
        .collect();
    assert!(sessions[..3].iter().all(|id| id.starts_with("gw-")));
    assert_eq!(sessions[0], sessions[1]);
    assert_ne!(sessions[0], sessions[2]);
    assert!(
        sessions[3..]
            .iter()
            .all(|id| uuid::Uuid::parse_str(id).is_ok())
    );
    assert_ne!(sessions[3], sessions[4]);
}

#[tokio::test]
async fn another_vendor_does_not_carry_opencode_sessions() {
    let openai = FakeOpenAi::start().await;
    let gateway = chat_gateway(&openai.base_url(), "openai");
    let mut req = post("/v1/chat/completions", CHAT);
    req.headers_mut().insert(
        "x-opencode-session",
        http::HeaderValue::from_static("session"),
    );
    send(&gateway, req).await;
    let calls = openai.requests();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].header("x-opencode-session"), None);
}
