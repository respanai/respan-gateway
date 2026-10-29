use super::*;
use respan_gateway_core::{Gateway, Provider};
use respan_gateway_http::{Body, ClientIo, connect, handle, http, http_body_util::BodyExt};
use respan_gateway_mcp_http::{SESSION, StreamableHttp};
use serde_json::json;
use std::sync::atomic::AtomicUsize;

async fn request(
    gateway: &Gateway,
    session: Option<&str>,
    value: Value,
) -> (Value, Option<String>) {
    let mut request = http::Request::builder()
        .method(http::Method::POST)
        .uri(ROOT)
        .header("content-type", "application/json")
        .header("accept", "application/json");
    if let Some(session) = session {
        request = request.header(SESSION, session);
    }
    let request = request
        .body(Body::full(value.to_string()))
        .expect("request");
    let (response, done) = handle(gateway, request, 4096).await;
    assert_eq!(response.status(), StatusCode::OK);
    let session = response
        .headers()
        .get(SESSION)
        .map(|s| s.to_str().expect("session").to_owned());
    let (body, end) = tokio::join!(BodyExt::collect(response.into_body()), done);
    end.expect("delivery");
    let value = serde_json::from_slice(&body.expect("body").to_bytes()).expect("JSON response");
    (value, session)
}

async fn switched(method: &'static str, params: Value, field: Option<&'static str>) {
    let switches = Arc::new(ToolSwitches::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let send = connect(
        move |request: http::Request<Body>| {
            let calls = seen.clone();
            async move {
                let bytes = BodyExt::collect(request.into_body())
                    .await
                    .expect("request body")
                    .to_bytes();
                let asked: Value = serde_json::from_slice(&bytes).expect("JSON request");
                let result = match asked["method"].as_str().expect("method") {
                    "initialize" => json!({
                        "protocolVersion": PROTOCOL,
                        "capabilities": {"tools": {}, "prompts": {}, "resources": {"subscribe": true}},
                        "serverInfo": {"name": "files", "version": "1"}
                    }),
                    "notifications/initialized" => {
                        return Ok((
                            http::Response::builder()
                                .status(StatusCode::ACCEPTED)
                                .body(Body::default())
                                .expect("notification response"),
                            None,
                        ));
                    }
                    received => {
                        assert_eq!(received, method);
                        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        if method == "prompts/get" {
                            assert_eq!(asked["params"]["name"], "read");
                        } else if method.starts_with("resources/") && field.is_none() {
                            assert_eq!(asked["params"]["uri"], "file:///a");
                        }
                        match field {
                            Some("prompts") => json!({"prompts": [{"name": "read"}]}),
                            Some("resources") => {
                                json!({"resources": [{"name": "read", "uri": "file:///a"}]})
                            }
                            Some("resourceTemplates") => {
                                json!({"resourceTemplates": [{"name": "read", "uriTemplate": "file:///{name}"}]})
                            }
                            _ if method == "prompts/get" => json!({"messages": []}),
                            _ if method == "resources/read" => json!({"contents": [{
                                "uri": "file:///a", "mimeType": "text/plain", "text": "contents"
                            }]}),
                            _ => json!({}),
                        }
                    }
                };
                let answer = json!({"jsonrpc": "2.0", "id": asked["id"], "result": result});
                Ok((
                    http::Response::builder()
                        .header("content-type", "application/json")
                        .body(Body::full(answer.to_string()))
                        .expect("response"),
                    None,
                ))
            }
        },
        4096,
    );
    let gateway = Gateway::new()
        .client::<Input, Output, ClientIo>(StreamableHttp::default())
        .expect("client")
        .provider(Provider::new("files", StreamableHttp::default(), send))
        .expect("provider")
        .plugin(McpRouter::with_switches(
            vec!["files".into()],
            ToolFilter::default(),
            switches.clone(),
        ));
    let (_, session) = request(&gateway, None, json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": PROTOCOL, "capabilities": {}, "clientInfo": {"name": "test", "version": "1"}}
    })).await;
    let session = session.expect("client session");
    let asked = json!({"jsonrpc": "2.0", "id": 2, "method": method, "params": params});
    let (enabled, _) = request(&gateway, Some(&session), asked.clone()).await;
    assert!(enabled.get("result").is_some(), "{enabled}");
    if let Some(field) = field {
        assert_eq!(enabled["result"][field].as_array().expect("items").len(), 1);
    }
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    switches.set_server("files", false);
    let (disabled, _) = request(&gateway, Some(&session), asked.clone()).await;
    match field {
        Some(field) => assert_eq!(disabled["result"][field], json!([])),
        None => assert_eq!(disabled["error"]["code"], -32602),
    }
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a disabled server was called"
    );

    switches.set_server("files", true);
    let (enabled_again, _) = request(&gateway, Some(&session), asked).await;
    assert_eq!(enabled_again, enabled);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_disabled_server_withholds_prompts() {
    switched("prompts/list", json!({}), Some("prompts")).await;
}

#[tokio::test]
async fn a_disabled_server_withholds_resources() {
    switched("resources/list", json!({}), Some("resources")).await;
}

#[tokio::test]
async fn a_disabled_server_withholds_resource_templates() {
    switched(
        "resources/templates/list",
        json!({}),
        Some("resourceTemplates"),
    )
    .await;
}

#[tokio::test]
async fn a_disabled_server_refuses_prompt_calls() {
    switched("prompts/get", json!({"name": "files__read"}), None).await;
}

#[tokio::test]
async fn a_disabled_server_refuses_resource_reads() {
    switched(
        "resources/read",
        json!({"uri": "mcp://files/file:///a"}),
        None,
    )
    .await;
}

#[tokio::test]
async fn a_disabled_server_refuses_resource_subscriptions() {
    switched(
        "resources/subscribe",
        json!({"uri": "mcp://files/file:///a"}),
        None,
    )
    .await;
}

#[tokio::test]
async fn a_disabled_server_refuses_resource_unsubscriptions() {
    switched(
        "resources/unsubscribe",
        json!({"uri": "mcp://files/file:///a"}),
        None,
    )
    .await;
}
