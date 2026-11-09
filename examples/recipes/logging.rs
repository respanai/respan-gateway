//! Request logs through a sink you choose, and zero data retention for
//! the tenants that ask for it.
//!
//! `Log` writes one record per provider call (`on_provider_end`) and one per
//! client request (`on_client_end`) to a [`Sink`]. Core has no request id,
//! so `Log` makes its own and keeps it in an extension, which every call
//! copies. Content reaches it only through what it asks for: the completion
//! through an output filter, the prompt through the input. Usage reaches it
//! through a filter on each call that sums what the provider reports, so a
//! call's record has usage and a request's has none.
//!
//! A tenant marked `NoContent` gets no content filter and no stored text;
//! usage and status, for billing and audit, still flow.
//!
//! Any plugin can remove an extension, the marker included: register the
//! plugin that sets it first, as here, and keep the type private. Caches and
//! stored responses are other plugins; each must check the same marker.
//!
//! Run it with `cargo run -p respan-gateway-examples --example logging`.
//! It sends one request as `acme` and one as `zdr-bank`, then prints the
//! records: look for the prompt and completion on `acme`'s request record
//! and none on `zdr-bank`'s, with usage on both call records.

use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use mockllm::{Answer, FakeOpenAi};
use respan_gateway_conversation::{Delta, Input, Message, Output, Usage};
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::{
    ClientContext, Extensions, Fault, Gateway, Plugin, Provider, ProviderContext, Response,
    async_trait,
};
use respan_gateway_http::Body;
use respan_gateway_http::http::{self, HeaderMap, HeaderValue};
use respan_gateway_http::http_body_util::BodyExt;
use respan_gateway_http::raw::Original;
use respan_gateway_http_transport::HttpTransport;

#[tokio::main]
async fn main() -> Result<(), Fault> {
    let fake = FakeOpenAi::start().await;
    let memory = Memory::default();
    let gateway = gateway(&fake.base_url(), memory.clone())?;

    for tenant in ["acme", "zdr-bank"] {
        fake.answer(Answer::text("Hello world"));
        let status = chat(&gateway, tenant).await?;
        println!("{tenant}: HTTP {status}");
        // End hooks run after the response; wait for them before reading.
        gateway.idle().await;
    }

    println!("\nrecords:");
    for r in memory.0.lock().iter() {
        let what = r
            .call
            .as_deref()
            .map_or("request".into(), |p| format!("call to {p}"));
        println!(
            "  #{} {what}: {} (status {:?}), usage in={} out={}",
            r.request, r.end, r.status, r.usage.input_tokens, r.usage.output_tokens
        );
        if r.call.is_none() {
            println!("      prompt {:?}, completion {:?}", r.prompt, r.completion);
        }
    }
    Ok(())
}

// The log.

/// One record: a request, or one of its calls (`call` names the provider).
#[derive(Debug, Clone)]
struct Record {
    request: u64,
    call: Option<String>,
    /// The HTTP status of the fault that ended it; none for a success.
    status: Option<u16>,
    /// `ok`, or the code of the fault that ended it.
    end: String,
    /// What the call's provider reported; zero for a request.
    usage: Usage,
    prompt: Option<String>,
    completion: Option<String>,
}

/// Where records go. End hooks await it, and a request's `on_client_end`
/// waits for its calls' end hooks: keep it fast or spawn. A Postgres insert
/// or a `tracing::info!(?record)` is one more implementation.
#[async_trait]
trait Sink: Send + Sync + 'static {
    async fn write(&self, record: Record);
}

/// Records in memory, for `main` to print.
#[derive(Clone, Default)]
struct Memory(Arc<Mutex<Vec<Record>>>);

#[async_trait]
impl Sink for Memory {
    async fn write(&self, record: Record) {
        self.0.lock().push(record);
    }
}

/// The request's tenant forbids storing prompts and completions.
#[derive(Clone, Copy)]
struct NoContent;

#[derive(Clone, Copy)]
struct RequestId(u64);

/// The completion's text, captured by the request's output filter.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<String>>);

/// One call's usage, summed by its provider-edge filter.
#[derive(Clone, Default)]
struct CallUsage(Arc<Mutex<Usage>>);

struct Log<S> {
    sink: S,
    next: AtomicU64,
}

impl Record {
    /// A record without content or usage, from what every hook sees.
    fn new(ext: &Extensions, call: Option<&str>, end: &Result<(), Fault>) -> Self {
        Self {
            request: ext.get::<RequestId>().map_or(0, |id| id.0),
            call: call.map(Into::into),
            status: end
                .as_ref()
                .err()
                .map(|f| respan_gateway_http_faults::status(f).as_u16()),
            end: end
                .as_ref()
                .map_or_else(|f| f.code().to_owned(), |()| "ok".into()),
            usage: Usage::default(),
            prompt: None,
            completion: None,
        }
    }
}

#[async_trait]
impl<S: Sink> Plugin for Log<S> {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>, Fault> {
        let id = RequestId(self.next.fetch_add(1, Ordering::Relaxed));
        cx.extensions_mut().insert(id);
        if cx.extensions().get::<NoContent>().is_some() || !cx.is_kind::<Input>() {
            return Ok(None);
        }
        let captured = Captured::default();
        let into = captured.0.clone();
        // The completion is the text deltas: reasoning and tool-call
        // arguments stay out of it.
        cx.filter(move |reply: &mut Output| {
            if let Output::Delta {
                delta: Delta::Text(text),
                ..
            } = reply
            {
                into.lock().push_str(text);
            }
            Ok(())
        })?;
        cx.extensions_mut().insert(captured);
        Ok(None)
    }

    async fn on_provider(&self, px: &mut ProviderContext) -> Result<(), Fault> {
        if !px.is_kind::<Input>() {
            return Ok(());
        }
        let usage = CallUsage::default();
        px.extensions_mut().insert(usage.clone());
        px.filter(move |reply: &mut Output| {
            if let Output::Usage(reported) = reply {
                *usage.0.lock() += &*reported;
            }
            Ok(())
        })
    }

    async fn on_provider_end(&self, px: &ProviderContext, end: &Result<(), Fault>) {
        let mut record = Record::new(px.extensions(), Some(px.provider()), end);
        if let Some(usage) = px.extensions().get::<CallUsage>() {
            record.usage = *usage.0.lock();
        }
        self.sink.write(record).await;
    }

    async fn on_client_end(&self, cx: &ClientContext, end: &Result<(), Fault>) {
        let ext = cx.extensions();
        let mut record = Record::new(ext, None, end);
        // Checked again: a marker set after `on_client` still holds.
        let captured = ext
            .get::<Captured>()
            .filter(|_| ext.get::<NoContent>().is_none());
        if let Some(captured) = captured {
            let prompt = cx.input::<Input>().ok().map(|chat| {
                let lines: Vec<String> = chat.messages.iter().map(Message::text).collect();
                lines.join("\n")
            });
            record.prompt = prompt;
            record.completion = Some(captured.0.lock().clone());
        }
        self.sink.write(record).await;
    }
}

/// Marks requests of zero-retention tenants, named by `x-tenant` on the
/// client's request as it came ([`Original`]); in practice, by the tenant
/// your auth plugin resolved.
struct Tenancy(&'static [&'static str]);

#[async_trait]
impl Plugin for Tenancy {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>, Fault> {
        let asked = cx.extensions().get::<Original>().and_then(Original::get);
        let tenant = asked.and_then(|a| a.headers().get("x-tenant")?.to_str().ok());
        if tenant.is_some_and(|t| self.0.contains(&t)) {
            cx.extensions_mut().insert(NoContent);
        }
        Ok(None)
    }
}

// The gateway and a client.

/// An OpenAI chat gateway in front of `base`, with `Tenancy` registered
/// before `Log` so the marker is set when `Log` looks.
fn gateway(base: &str, sink: impl Sink) -> Result<Gateway, Fault> {
    let mut headers = HeaderMap::new();
    headers.insert("authorization", HeaderValue::from_static("Bearer sk-test"));
    let transport = HttpTransport::new(base)?.with_headers(headers)?.connect();
    let log = Log {
        sink,
        next: AtomicU64::new(1),
    };
    Gateway::new()
        .provider(Provider::new("openai", OpenAiChat, transport))?
        .client::<Input, _, _>(OpenAiChat)?
        .plugin(Tenancy(&["zdr-bank"]))
        .plugin(log)
        .target("openai")
}

/// A streamed chat request as `tenant`, read to its end; its HTTP status.
async fn chat(gateway: &Gateway, tenant: &'static str) -> Result<u16, Fault> {
    let body = r#"{"model":"gpt-x","stream":true,"stream_options":{"include_usage":true},
        "messages":[{"role":"user","content":"hi"}]}"#;
    let mut req = http::Request::new(Body::full(body));
    *req.method_mut() = http::Method::POST;
    *req.uri_mut() = http::Uri::from_static("/v1/chat/completions");
    req.headers_mut()
        .insert("x-tenant", HeaderValue::from_static(tenant));
    let (resp, _done) = respan_gateway_http::handle(gateway, req, 32 << 20).await;
    let status = resp.status().as_u16();
    resp.into_body().collect().await?;
    Ok(status)
}
