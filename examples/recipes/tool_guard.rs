//! A tool allow-list for any realtime session, whatever its formats. A call of
//! a tool outside the list never reaches the client: the model hears that the
//! tool is not allowed and goes on. A session offers the model the allowed
//! tools alone. The guard reads the realtime kind's events, so it guards an
//! OpenAI Realtime client answered by Gemini Live as it would any other.
//!
//! This is a live-provider server; it costs Gemini Live usage per session.
//!
//! ```bash
//! GEMINI_API_KEY=... ALLOWED_TOOLS=get_weather,get_time \
//!   cargo run -p respan-gateway-examples --example tool_guard
//! ```
//!
//! It listens at 127.0.0.1:8083. Clients name their model at `?model=`, as
//! `ws://127.0.0.1:8083/v1/realtime?model=gemini-live-2.5-flash`. Offer tools
//! inside and outside the list and ask for both: only the allowed calls
//! arrive. Ctrl-C closes active sessions before exiting.
use std::sync::Arc;

use respan_gateway_core::futures::channel::mpsc;
use respan_gateway_core::futures::{FutureExt, Stream, StreamExt, future, stream};
use respan_gateway_core::{
    ClientContext, EventStream, Fault, Filter, Gateway, Plugin, Provider, Response, async_trait,
};
use respan_gateway_realtime::{Input, Output, ToolResult};
use respan_gateway_realtime_ws::{GeminiLive, OpenAiRealtime};
use respan_gateway_ws::WsTransport;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut headers = respan_gateway_http::http::HeaderMap::new();
    headers.insert("x-goog-api-key", std::env::var("GEMINI_API_KEY")?.parse()?);
    let allowed = std::env::var("ALLOWED_TOOLS")?;
    let allowed: Vec<&str> = allowed.split(',').map(str::trim).collect();
    let transport = GeminiLive
        .transport("wss://generativelanguage.googleapis.com")?
        .with_headers(headers)?;
    serve(gateway(transport, &allowed)?, "127.0.0.1:8083").await?;
    Ok(())
}

/// Clients of OpenAI Realtime and Gemini Live, answered by Gemini Live at
/// `transport`, guarded to `allowed`.
fn gateway(transport: WsTransport, allowed: &[&str]) -> Result<Gateway, Fault> {
    let allowed = allowed.iter().map(|t| (*t).to_owned()).collect();
    Gateway::new()
        .client::<Input, _, _>(OpenAiRealtime)?
        .client::<Input, _, _>(GeminiLive)?
        .provider(Provider::new("gemini", GeminiLive, transport.connect()))?
        .plugin(ToolGuard { allowed })
        .target("gemini")
}

/// What the model hears of a call it may not make.
const REFUSED: &str = r#"{"error":"this tool is not allowed"}"#;

/// The guard: the tools a session may call.
#[derive(Clone)]
struct ToolGuard {
    allowed: Arc<[String]>,
}

#[async_trait]
impl Plugin for ToolGuard {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>, Fault> {
        // The session offers only the allowed tools.
        let allowed = self.allowed.clone();
        cx.filter(move |input: &mut Input| {
            if let Input::Session(session) = input
                && let Some(tools) = &mut session.tools
            {
                tools.retain(|tool| allowed.contains(&tool.name));
            }
            Ok(())
        })?;
        // Refusals go to the model among what the client says.
        let (tell, told) = mpsc::unbounded();
        cx.wrap(move |inputs: EventStream<Input>| among(inputs, told))?;
        cx.filter(Refuse {
            allowed: self.allowed.clone(),
            tell,
            refused: false,
            called: false,
        })?;
        Ok(None)
    }
}

/// What the client says, and what the guard tells the model among it, until
/// the client's end.
fn among(
    inputs: EventStream<Input>,
    told: mpsc::UnboundedReceiver<Input>,
) -> impl Stream<Item = Result<Input, Fault>> {
    let inputs = inputs.map(Some).chain(stream::once(future::ready(None)));
    stream::select(inputs, told.map(|input| Some(Ok(input))))
        .take_while(|input| future::ready(input.is_some()))
        .filter_map(future::ready)
}

/// Drops the calls of tools not allowed, answering each for the client; a
/// response whose calls were all refused is asked for again, without them.
struct Refuse {
    allowed: Arc<[String]>,
    tell: mpsc::UnboundedSender<Input>,
    /// Whether the response being made called a tool not allowed.
    refused: bool,
    /// Whether it called one allowed, which its client answers.
    called: bool,
}

impl Filter<Output> for Refuse {
    fn edit(&mut self, output: Output, out: &mut Vec<Output>) -> Result<(), Fault> {
        match &output {
            Output::ToolCall { id, name, .. } if !self.allowed.contains(name) => {
                let answer = ToolResult::new(id.clone(), REFUSED);
                let _ = self.tell.unbounded_send(Input::ToolResult(answer));
                self.refused = true;
                return Ok(());
            }
            Output::ToolCall { .. } => self.called = true,
            Output::ResponseDone { .. } => {
                let called = std::mem::take(&mut self.called);
                if std::mem::take(&mut self.refused) && !called {
                    let _ = self.tell.unbounded_send(Input::Respond { native: None });
                }
            }
            _ => {}
        }
        out.push(output);
        Ok(())
    }
}

/// Serve `gateway` at `address` until Ctrl-C, then close every upgraded
/// session and wait for its hooks before returning.
async fn serve(gateway: Gateway, address: &str) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(address).await?;
    let stop = async {
        let _ = tokio::signal::ctrl_c().await;
    }
    .boxed()
    .shared();
    let app = respan_gateway_serve::Host::new(gateway.clone())
        .limit(|_| 32 << 20)
        .limits(respan_gateway_ws::Limits::default())
        .stop(stop.clone())
        .router();
    axum::serve(listener, app)
        .with_graceful_shutdown(stop)
        .await?;
    gateway.idle().await;
    Ok(())
}
