//! The gateway as a library: another Rust service runs the pipeline in
//! process, with no server and no second hop.
//!
//! The service builds a [`Gateway`], hands [`Gateway::handle`] a request on
//! the HTTP wire (a `ClientIo` made from an owned `http::Request`), reads the
//! answer and awaits how the request ended. No HTTP hop means none of the
//! adapter's hop rules apply; the service still passes the gateway what the
//! `ClientIo` publishes (`ClientIo::extensions`), as the adapter does. No
//! answer means no format took the request, and the end says why.
//!
//! The kernel counts no usage. The service tallies it with a small plugin,
//! `Meter`, that sums what each call's provider reports, as billing would.
//!
//! Run it with `cargo run -p respan-gateway-examples --example embedded`.
//! Look for the status, the provider's body as the client gets it, and the
//! usage the meter summed.

use mockllm::{Answer, FakeOpenAi};
use parking_lot::Mutex;
use respan_gateway_conversation::{Input, Output, Usage};
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::futures::future::join;
use respan_gateway_core::futures::{StreamExt, stream};
use respan_gateway_core::{Fault, Gateway, Plugin, Provider, ProviderContext, async_trait};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::{self, HeaderValue};
use respan_gateway_http::http_body_util::BodyExt;
use respan_gateway_http_transport::HttpTransport;
use std::sync::Arc;

/// The most bytes of one message the adapter holds.
const LIMIT: usize = 32 << 20;

#[tokio::main]
async fn main() -> Result<(), Fault> {
    let openai = FakeOpenAi::start().await;
    openai.answer(Answer::raw(ANSWER));

    let meter = Meter::default();
    let gw = gateway(provider(&openai)?, meter.clone())?;
    let (status, body, end) = ask(&gw, "Say hi").await?;
    // End hooks run detached; wait for them before reading the meter.
    gw.idle().await;

    println!("Asked the in-process gateway one chat question, with no server.");
    println!("status: {status}, ended: {end:?}");
    println!("body:   {}", String::from_utf8_lossy(&body));
    println!("usage:  {:?}", meter.usage());
    Ok(())
}

/// The gateway: one OpenAI provider, OpenAI chat for clients, and the meter.
fn gateway(provider: HttpTransport, meter: Meter) -> Result<Gateway, Fault> {
    Gateway::new()
        .provider(Provider::new("openai", OpenAiChat, provider.connect()))?
        .client::<Input, _, _>(OpenAiChat)?
        .plugin(meter)
        .target("openai")
}

/// The scripted provider. For real use, the same transport points at
/// `https://api.openai.com/v1` with a real key.
fn provider(openai: &FakeOpenAi) -> Result<HttpTransport, Fault> {
    let mut headers = http::HeaderMap::new();
    headers.insert("authorization", HeaderValue::from_static("Bearer sk-test"));
    HttpTransport::new(openai.base_url())?.with_headers(headers)
}

/// One chat request through the pipeline: the answer's status and body,
/// and how the request ended.
async fn ask(
    gw: &Gateway,
    prompt: &str,
) -> Result<(http::StatusCode, Bytes, Result<(), Fault>), Fault> {
    let body = serde_json::json!({
        "model": "gpt-6-luna",
        "messages": [{"role": "user", "content": prompt}],
    });
    let (head, ()) = http::Request::post("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(())
        .map_err(respan_gateway_http_faults::built_wrong)?
        .into_parts();
    let body = stream::iter([Ok(Bytes::from(body.to_string()))]).boxed();
    let request = http::Request::from_parts(head, respan_gateway_http::Body::stream(body));

    let (incoming, answer) = respan_gateway_http::ClientIo::accept(request, LIMIT);
    let extensions = incoming.extensions();
    let handled = gw.handle(incoming, extensions);
    let consume = async {
        let Some((resp, end)) = answer.await else {
            return Ok::<_, Fault>(None);
        };
        let (head, body) = resp.into_parts();
        let body = BodyExt::collect(body).await?.to_bytes();
        Ok(Some((head.status, body, end.await)))
    };
    // Handling includes delivery, so drive it while reading the answer.
    let (handled, answer) = join(handled, consume).await;
    answer?.ok_or_else(|| {
        handled
            .err()
            .unwrap_or_else(|| Fault::internal("no format answered"))
    })
}

/// Sums the usage every chat call's provider reports. Clones share it.
#[derive(Debug, Clone, Default)]
struct Meter(Arc<Mutex<Usage>>);

impl Meter {
    fn usage(&self) -> Usage {
        *self.0.lock()
    }
}

#[async_trait]
impl Plugin for Meter {
    async fn on_provider(&self, px: &mut ProviderContext) -> Result<(), Fault> {
        if !px.is_kind::<Input>() {
            return Ok(());
        }
        // Output filters run the last added first: register the meter last,
        // so it sees what every other plugin let through.
        let total = self.0.clone();
        px.filter(move |reply: &mut Output| {
            if let Output::Usage(reported) = reply {
                *total.lock() += &*reported;
            }
            Ok(())
        })
    }
}

const ANSWER: &str = r#"{"id":"c1","object":"chat.completion","model":"gpt-6-luna","choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":"hi"}}],"usage":{"prompt_tokens":3,"completion_tokens":1}}"#;
