use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::{Gateway, Provider};
use respan_gateway_http::{Body, bytes::Bytes, connect, handle, http, http_body_util::BodyExt};
use serde_json::{Value, json};

const CHAT: &str = "/v1/chat/completions";
const LIMIT: usize = 16 * 1024;

async fn send(
    path: &str,
    input: Value,
    upstream: impl Fn(Value) -> (u16, &'static str, String) + Send + Sync + 'static,
) -> (u16, String) {
    let upstream = std::sync::Arc::new(upstream);
    let transport = connect(
        move |request| {
            let upstream = upstream.clone();
            mock(request, move |body| upstream(body))
        },
        LIMIT,
    );
    let gateway = Gateway::new()
        .client::<_, _, respan_gateway_http::ClientIo>(OpenAiChat)
        .expect("client format")
        .provider(Provider::new("mock", OpenAiChat, transport))
        .expect("mock provider")
        .target("mock")
        .expect("default target");
    let request = http::Request::post(path)
        .header("content-type", "application/json")
        .body(Body::full(Bytes::from(input.to_string())))
        .expect("HTTP request");
    let (reply, completion) = handle(&gateway, request, LIMIT).await;
    let status = reply.status().as_u16();
    let raw = reply
        .into_body()
        .collect()
        .await
        .expect("response body")
        .to_bytes();
    let _ = completion.await;
    (
        status,
        String::from_utf8(raw.to_vec()).expect("UTF-8 response"),
    )
}

async fn mock(
    request: http::Request<Body>,
    upstream: impl Fn(Value) -> (u16, &'static str, String),
) -> respan_gateway_http::Sent {
    let raw = request.into_body().collect().await?.to_bytes();
    let (status, media, body) = upstream(serde_json::from_slice(&raw).expect("upstream JSON"));
    let reply = http::Response::builder()
        .status(status)
        .header("content-type", media)
        .body(Body::full(Bytes::from(body)))
        .expect("mock response");
    Ok((reply, None))
}

#[tokio::test]
async fn two_openai_choices_reach_a_unary_client() {
    let input = json!({"model":"gpt-4o", "n":2, "messages":[{"role":"user","content":"Hi"}]});
    let (status, body) = send(CHAT, input, |sent| {
        assert_eq!(sent["n"], 2);
        (
            200,
            "application/json",
            json!({"id":"c", "model":"gpt-4o", "choices":[
                {"index":0,"message":{"role":"assistant","content":"A"},"finish_reason":"stop"},
                {"index":1,"message":{"role":"assistant","content":"B"},"finish_reason":"stop"}
            ]})
            .to_string(),
        )
    })
    .await;
    assert_eq!(status, 200, "{body}");
    let reply: Value = serde_json::from_str(&body).expect("response JSON");
    assert_eq!(reply["choices"].as_array().expect("choices array").len(), 2);
    assert_eq!(reply["choices"][0]["message"]["content"], "A");
    assert_eq!(reply["choices"][1]["message"]["content"], "B");
}

#[tokio::test]
async fn two_openai_choices_reach_a_streaming_client() {
    let input = json!({"model":"gpt-4o", "n":2, "stream":true,
        "messages":[{"role":"user","content":"Hi"}]});
    let (status, body) = send(CHAT, input, |sent| {
        assert_eq!(sent["n"], 2);
        assert_eq!(sent["stream"], true);
        let chunk = json!({"id":"c", "model":"gpt-4o", "choices":[
            {"index":1,"delta":{"role":"assistant","content":"B"},"finish_reason":"stop"},
            {"index":0,"delta":{"role":"assistant","content":"A"},"finish_reason":"stop"}
        ]});
        (
            200,
            "text/event-stream",
            format!("data: {chunk}\n\ndata: [DONE]\n\n"),
        )
    })
    .await;
    assert_eq!(status, 200, "{body}");
    let mut contents = [String::new(), String::new()];
    for chunk in body.lines().filter_map(|line| line.strip_prefix("data: ")) {
        let Ok(value) = serde_json::from_str::<Value>(chunk) else {
            continue;
        };
        for choice in value["choices"].as_array().into_iter().flatten() {
            let index = choice["index"].as_u64().expect("choice index") as usize;
            contents[index].push_str(choice["delta"]["content"].as_str().unwrap_or(""));
        }
    }
    assert_eq!(contents, ["A", "B"]);
    assert!(body.ends_with("data: [DONE]\n\n"));
}
