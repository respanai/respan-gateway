#![allow(dead_code, clippy::unwrap_used)]
use respan_gateway_conversation::Input;
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::{Gateway, Provider};
use respan_gateway_http::{
    Body,
    http::{HeaderMap, HeaderValue, Method, Request},
    http_body_util::BodyExt,
};
use respan_gateway_http_transport::HttpTransport;
use respan_gateway_plugin_ark::Ark;
use serde_json::Value;

pub fn gateway(url: &str, guard: Option<&Ark>) -> Gateway {
    let mut headers = HeaderMap::new();
    headers.insert("authorization", HeaderValue::from_static("Bearer sk-test"));
    let gateway = Gateway::new()
        .provider(Provider::new(
            "local",
            OpenAiChat,
            HttpTransport::new(url)
                .unwrap()
                .with_headers(headers)
                .unwrap()
                .connect(),
        ))
        .unwrap()
        .client::<Input, _, _>(OpenAiChat)
        .unwrap()
        .target("local")
        .unwrap();
    match guard {
        Some(guard) => gateway.plugin(guard.clone()).plugin(guard.provider_guard()),
        None => gateway,
    }
}
pub fn text_body(text: &str) -> Value {
    serde_json::json!({"model":"test","messages":[{"role":"user","content":text}]})
}
pub async fn send(gateway: &Gateway, value: Value) -> (u16, String) {
    let mut request = Request::new(Body::full(value.to_string()));
    *request.method_mut() = Method::POST;
    *request.uri_mut() = "/v1/chat/completions".parse().unwrap();
    let (response, ended) = respan_gateway_http::handle(gateway, request, 1 << 20).await;
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let _ = ended.await;
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}
