//! A local proxy in front of Claude Code: no config file, one Anthropic
//! provider, and the client's own credentials (API key or subscription
//! token) passed on by the `Relay` plugin, since the transport sets no
//! headers of its own.
//!
//! The request is decoded and written again in its own format, which keeps
//! its spec's beta headers and query and the fields it has no form for. A
//! response is decoded into events and written again, so an unknown event
//! survives. `Relay` also gives back the vendor's response head and error
//! bodies. The binary does the same, and also relays the vendor's other
//! endpoints: `respan-gateway proxy --anthropic`.
//!
//! Run it offline with
//! `cargo run -p respan-gateway-examples --example local_proxy`: one
//! Claude Code request goes through against a scripted reply. Look for the
//! unknown request fields reaching the provider, the client's token and beta
//! header passed on, and the unknown `brand_new_event` in the reply.
//!
//! Add `-- --serve 127.0.0.1:8080` to listen for real, then run
//! `ANTHROPIC_BASE_URL=http://127.0.0.1:8080 claude`. Set
//! `RESPAN_ANTHROPIC_UPSTREAM` to use another upstream than Anthropic's.

use parking_lot::Mutex;
use respan_gateway_conversation::Input;
use respan_gateway_conversation_http::anthropic::AnthropicMessages;
use respan_gateway_core::futures::TryStreamExt;
use respan_gateway_core::{Extensions, Fault, Gateway, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::{self, HeaderMap, HeaderValue};
use respan_gateway_http::http_body_util::BodyExt;
use respan_gateway_http::{Body, ProviderIo};
use respan_gateway_http_transport::HttpTransport;
use respan_gateway_plugin_relay::Relay;
use std::sync::Arc;

/// The most bytes of one message the adapter and transports hold.
const LIMIT: usize = 32 << 20;
const ANTHROPIC: &str = "https://api.anthropic.com";

/// What the provider received: path, headers and body.
type Sent = Arc<Mutex<Vec<(http::Uri, HeaderMap, Bytes)>>>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => offline().await,
        [flag, address] if flag == "--serve" => serve(address).await,
        _ => Err("usage: local_proxy [--serve ADDRESS]".into()),
    }
}

/// The whole setup, in code: serve Anthropic Messages, relay to Anthropic.
fn gateway(
    transport: impl Fn(Extensions) -> ProviderIo + Send + Sync + 'static,
) -> Result<Gateway, Fault> {
    Ok(Gateway::new()
        .provider(Provider::new("anthropic", AnthropicMessages, transport))?
        .client::<Input, _, _>(AnthropicMessages)?
        .target("anthropic")?
        .plugin(Relay))
}

/// Listen for real. No `with_headers`: each request carries the developer's
/// own key.
async fn serve(address: &str) -> Result<(), Box<dyn std::error::Error>> {
    let upstream =
        std::env::var("RESPAN_ANTHROPIC_UPSTREAM").unwrap_or_else(|_| ANTHROPIC.to_owned());
    let gw = gateway(HttpTransport::new(&upstream)?.connect())?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    let bound = listener.local_addr()?;
    println!("listening on {bound}; ANTHROPIC_BASE_URL=http://{bound} claude");
    axum::serve(listener, respan_gateway_serve::router(gw)).await?;
    Ok(())
}

/// One Claude Code request through the proxy, against a scripted provider.
async fn offline() -> Result<(), Box<dyn std::error::Error>> {
    let sent = Sent::default();
    let gw = gateway(scripted(STREAM.concat(), sent.clone()))?;

    let (resp, _end) = respan_gateway_http::handle(&gw, request(), LIMIT).await;
    let (head, body) = resp.into_parts();
    let body = BodyExt::collect(body).await?.to_bytes();

    let sent = sent.lock();
    let (uri, headers, forwarded) = &sent[0];
    println!("Claude Code -> proxy -> Anthropic, offline.");
    println!("provider got:  POST {uri}");
    for name in ["authorization", "anthropic-beta", "anthropic-version"] {
        println!("  {name}: {:?}", headers.get(name));
    }
    // Written again by the format: unknown fields kept, spacing not.
    println!("  body: {}", String::from_utf8_lossy(forwarded));
    println!("client got:    {}", head.status);
    print!("{}", String::from_utf8_lossy(&body));
    Ok(())
}

/// A provider that records what it was sent and answers `body`.
/// `credentials` lifts the client's own credentials onto the call, as
/// `HttpTransport` does with no headers of its own.
fn scripted(
    body: impl Into<Bytes>,
    sent: Sent,
) -> impl Fn(Extensions) -> ProviderIo + Send + Sync + 'static {
    let body = body.into();
    respan_gateway_http::connect(
        move |mut call| {
            let (body, sent) = (body.clone(), sent.clone());
            async move {
                respan_gateway_http::call::credentials(&mut call, &HeaderMap::new());
                let (head, request) = call.into_parts();
                let chunks: Vec<Bytes> = request.try_collect().await?;
                sent.lock()
                    .push((head.uri, head.headers, chunks.concat().into()));
                let response = http::Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::full(body))
                    .map_err(|e| Fault::internal(e.to_string()))?;
                Ok((response, None))
            }
        },
        LIMIT,
    )
}

/// What Claude Code sends: a subscription token, a beta header, and fields
/// the gateway has never heard of, spaced as the client spaced them.
const REQUEST: &str = r#"{"model":"claude-haiku-5-5", "max_tokens":1024,"stream":true,
 "messages":[{"role":"user","content":"hi"}],
 "thinking":{"type":"enabled","budget_tokens":2048}, "future_field":[1, 2.50]}"#;

fn request() -> http::Request<Body> {
    let mut req = http::Request::new(Body::full(REQUEST));
    *req.method_mut() = http::Method::POST;
    *req.uri_mut() = http::Uri::from_static("/v1/messages?beta=true");
    for (name, value) in [
        ("authorization", "Bearer sk-ant-oat-subscription"),
        ("anthropic-beta", "interleaved-thinking-2025-05-14"),
        ("anthropic-version", "2023-06-01"),
        ("content-type", "application/json"),
    ] {
        req.headers_mut()
            .insert(name, HeaderValue::from_static(value));
    }
    req
}

/// A stream with an event type and a field no format knows.
const STREAM: &[&str] = &[
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-haiku-5-5\",\"content\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
    "event: brand_new_event\ndata: {\"type\":\"brand_new_event\",\"x\":1}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\",\"citations_v9\":[]}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
];
