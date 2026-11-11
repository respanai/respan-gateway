//! Responses over WebSocket through the gateway: independent named lanes,
//! continuation IDs and per-turn billing. Clients send ordinary
//! `response.create` events; the gateway pins the model, caps output, and
//! holds each lane's turn against an account before it reaches the provider,
//! settling it at the usage `response.completed` reports. Unknown fields and
//! `previous_response_id` pass through, so continuation works as it does
//! upstream. A socket failure never replays a turn. Sessions end after 30
//! minutes.
//!
//! This is a live-provider server: each turn costs OpenAI usage.
//!
//! ```bash
//! OPENAI_API_KEY=... MODEL=gpt-4.1-mini MODEL_INPUT_LIMIT=1047576 \
//!   cargo run -p respan-gateway-examples --example responses_ws
//! ```
//!
//! It listens at 127.0.0.1:8081. MODEL_INPUT_LIMIT is the model's full input
//! bound: each turn holds that plus the output cap from a 5,000,000-token
//! account. Each finished turn prints its lane, response id, status, usage
//! and the account's balance. A second `response.create` on a lane still
//! answering closes the session with 1008; use `stream_id` to run lanes side
//! by side. Ctrl-C closes active sessions, charging cut turns their hold.
//! Protocol: https://developers.openai.com/api/docs/guides/websocket-mode
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use respan_gateway_core::futures::{FutureExt, StreamExt, TryStreamExt};
use respan_gateway_core::{
    ClientContext, ErrorCode, EventStream, Fault, Gateway, Plugin, Provider, Response, async_trait,
};
use respan_gateway_realtime::{Input, Output};
use respan_gateway_realtime_ws::ResponsesWs;
use respan_gateway_ws::{CloseCode, WsTransport};
use serde_json::{Value, json};

/// The most output tokens a turn may make.
const OUTPUT: i64 = 1_024;
/// How long a session may last.
const SESSION_MAX: Duration = Duration::from_secs(1_800);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut headers = respan_gateway_http::http::HeaderMap::new();
    let key = std::env::var("OPENAI_API_KEY")?;
    headers.insert("authorization", format!("Bearer {key}").parse()?);
    let transport = ResponsesWs
        .transport("wss://api.openai.com/v1")?
        .with_headers(headers)?;
    let sessions = Sessions {
        account: Arc::new(Mutex::new(Account {
            limit: 5_000_000,
            ..Account::default()
        })),
        model: std::env::var("MODEL")?,
        input_limit: std::env::var("MODEL_INPUT_LIMIT")?.parse()?,
    };
    serve(gateway(transport, sessions)?, "127.0.0.1:8081").await?;
    Ok(())
}

/// Responses WebSocket clients, answered by OpenAI at `transport`.
fn gateway(transport: WsTransport, sessions: Sessions) -> Result<Gateway, Fault> {
    Gateway::new()
        .client::<Input, _, _>(ResponsesWs)?
        .provider(Provider::new("responses", ResponsesWs, transport.connect()))?
        .plugin(sessions)
        .target("responses")
}

/// The plugin: one budget per session, over a shared account.
#[derive(Clone)]
struct Sessions {
    account: Arc<Mutex<Account>>,
    model: String,
    input_limit: i64,
}

#[async_trait]
impl Plugin for Sessions {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>, Fault> {
        if self.model.trim().is_empty() {
            return Err(Fault::config("configure an allowed model"));
        }
        let budget = Arc::new(Budget::new(self.account.clone(), self.input_limit)?);
        let model = self.model.clone();
        let incoming = budget.clone();
        cx.wrap(move |events: EventStream<Input>| {
            events.and_then(move |mut event| {
                let result = admit(&incoming, &model, &mut event).map(|()| event);
                std::future::ready(result)
            })
        })?;
        let outgoing = budget.clone();
        cx.wrap(move |events: EventStream<Output>| {
            events.and_then(move |event| {
                settle(&outgoing, &event);
                std::future::ready(Ok(event))
            })
        })?;
        cx.wrap(|events: EventStream<Output>| events.take_until(tokio::time::sleep(SESSION_MAX)))?;
        cx.extensions_mut().insert(budget);
        Ok(None)
    }

    async fn on_client_end(&self, cx: &ClientContext, _: &Result<(), Fault>) {
        if let Some(budget) = cx.extensions().get::<Arc<Budget>>() {
            budget.end();
        }
    }
}

/// Pin a `response.create` to `model`, cap its output and reserve its lane's
/// turn. One with `generate: false` only warms the cache, and costs nothing.
fn admit(budget: &Budget, model: &str, event: &mut Input) -> Result<(), Fault> {
    let Input::Native(native) = event else {
        return Ok(());
    };
    let json = Arc::make_mut(&mut native.value);
    if json["type"] != "response.create" {
        return Ok(());
    }
    if json.get("model").is_some_and(|v| v.as_str() != Some(model)) {
        return Err(Fault::bad_request(
            "model is outside the configured budget policy",
        ));
    }
    json["model"] = model.into();
    if json["generate"] == false {
        return Ok(());
    }
    let cap = &mut json["max_output_tokens"];
    if cap.as_i64().is_none_or(|n| !(1..=OUTPUT).contains(&n)) {
        *cap = OUTPUT.into();
    }
    budget.begin(json["stream_id"].as_str().unwrap_or("default"))
}

/// At a response's end, settle its lane's turn at the usage it reports.
fn settle(budget: &Budget, event: &Output) {
    let Output::Native(native) = event else {
        return;
    };
    let json = &native.value;
    let kind = json["type"].as_str().unwrap_or_default();
    if !matches!(
        kind,
        "response.completed" | "response.failed" | "response.incomplete"
    ) {
        return;
    }
    let lane = json["stream_id"].as_str().unwrap_or("default");
    let response = &json["response"];
    let used = response["usage"]["total_tokens"].as_i64();
    let record = json!({
        "lane": lane, "id": response["id"], "status": kind, "usage": response["usage"]
    });
    budget.finish(lane, used, record);
}

/// What an account may spend, has spent and holds for turns in flight.
#[derive(Debug, Default)]
struct Account {
    limit: i64,
    spent: i64,
    held: i64,
}

/// A session's turns in flight, by lane: each holds the most a turn may
/// cost (the full input bound and the output cap) until it settles.
struct Budget {
    account: Arc<Mutex<Account>>,
    quote: i64,
    lanes: Mutex<HashSet<String>>,
}

impl Budget {
    fn new(account: Arc<Mutex<Account>>, input_limit: i64) -> Result<Self, Fault> {
        let quote = input_limit
            .checked_add(OUTPUT)
            .filter(|_| input_limit > 0)
            .ok_or_else(|| Fault::config("model input-token bound must be positive and finite"))?;
        Ok(Self {
            account,
            quote,
            lanes: Mutex::default(),
        })
    }

    /// Hold a turn's quote on `lane`, or refuse it: one turn per lane, and
    /// none the account cannot cover.
    fn begin(&self, lane: &str) -> Result<(), Fault> {
        let mut lanes = self.lanes.lock();
        if lanes.contains(lane) {
            return Err(Fault::new("this lane already has a turn")
                .with(ErrorCode("turn_busy".into()))
                .with(CloseCode(1008)));
        }
        let mut account = self.account.lock();
        if account.limit - account.spent - account.held < self.quote {
            return Err(Fault::new("the account cannot cover this turn")
                .with(ErrorCode("insufficient_funds".into()))
                .with(CloseCode(1008)));
        }
        account.held += self.quote;
        lanes.insert(lane.to_owned());
        Ok(())
    }

    /// Settle `lane`'s turn at what it used, or at its quote if the
    /// provider never said.
    fn finish(&self, lane: &str, used: Option<i64>, record: Value) {
        if self.lanes.lock().remove(lane) {
            self.settle(used.unwrap_or(self.quote), &record);
        }
    }

    /// Settle the turns a closed session cut, at their quote: each was sent.
    fn end(&self) {
        for lane in std::mem::take(&mut *self.lanes.lock()) {
            self.settle(self.quote, &json!({"lane": lane, "status": "cut"}));
        }
    }

    fn settle(&self, spent: i64, record: &Value) {
        let mut account = self.account.lock();
        account.held -= self.quote;
        account.spent += spent;
        println!("turn {record}; {account:?}");
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
