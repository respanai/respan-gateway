//! Gemini Live through the gateway, with each turn paid for before the model
//! speaks. Clients send their setup, then `clientContent` turns or manual
//! activity boundaries (`realtimeInput.activityStart`). The gateway pins the
//! model, caps output, turns automatic activity detection and proactive audio
//! off, so every generation starts at a turn the gateway saw and reserved.
//! Tool calls, audio, interruptions, goAway and session-resumption handles
//! pass as they came. Sessions end after 30 minutes.
//!
//! This is a live-provider server: each turn costs Gemini Live usage.
//!
//! ```bash
//! GEMINI_API_KEY=... MODEL=gemini-live-2.5-flash MODEL_INPUT_LIMIT=1048576 \
//!   cargo run -p respan-gateway-examples --example gemini_live
//! ```
//!
//! It listens at 127.0.0.1:8082. MODEL_INPUT_LIMIT is the model's full input
//! bound: each turn holds that plus the output cap from a 5,000,000-token
//! account, and settles at the usage the provider reports. Each finished turn
//! prints its status, usage and the account's balance. Audio sent before
//! `activityStart` closes the session with 1008; Ctrl-C closes active
//! sessions, charging cut turns their hold.
//! Protocol: https://ai.google.dev/api/live
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use respan_gateway_core::futures::{FutureExt, StreamExt, TryStreamExt};
use respan_gateway_core::{
    ClientContext, ErrorCode, EventStream, Fault, Gateway, NativeFields, Plugin, Provider,
    Response, async_trait,
};
use respan_gateway_realtime::{Input, Output, Session, Status, Turns};
use respan_gateway_realtime_ws::GeminiLive;
use respan_gateway_ws::{CloseCode, WsTransport};
use serde_json::{Value, json};

/// The most output tokens a turn may make.
const OUTPUT: i64 = 1_024;
/// How long a session may last.
const SESSION_MAX: Duration = Duration::from_secs(1_800);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut headers = respan_gateway_http::http::HeaderMap::new();
    headers.insert("x-goog-api-key", std::env::var("GEMINI_API_KEY")?.parse()?);
    let transport = GeminiLive
        .transport("wss://generativelanguage.googleapis.com")?
        .with_headers(headers)?;
    let sessions = Sessions {
        account: Arc::new(Mutex::new(Account {
            limit: 5_000_000,
            ..Account::default()
        })),
        model: std::env::var("MODEL")?,
        input_limit: std::env::var("MODEL_INPUT_LIMIT")?.parse()?,
    };
    serve(gateway(transport, sessions)?, "127.0.0.1:8082").await?;
    Ok(())
}

/// Gemini Live clients, answered by Gemini Live at `transport`.
fn gateway(transport: WsTransport, sessions: Sessions) -> Result<Gateway, Fault> {
    Gateway::new()
        .client::<Input, _, _>(GeminiLive)?
        .provider(Provider::new("gemini", GeminiLive, transport.connect()))?
        .plugin(sessions)
        .target("gemini")
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
        let model = self.model.trim_start_matches("models/").to_owned();
        let incoming = budget.clone();
        cx.wrap(move |events: EventStream<Input>| {
            events.and_then(move |mut event| {
                let result = admit(&incoming, &model, &mut event).map(|()| event);
                std::future::ready(result)
            })
        })?;
        let outgoing = budget.clone();
        let turn = Arc::new(Mutex::new(Turn::default()));
        cx.wrap(move |events: EventStream<Output>| {
            events.and_then(move |event| {
                if let Some((used, record)) = said(&turn, &event) {
                    outgoing.finish("turn", used, record);
                }
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

/// Configure the setup, and reserve a turn where one begins: at its
/// activity, or at what asks for an answer (a complete turn, a tool's
/// answer). Realtime input outside a turn is refused.
fn admit(budget: &Budget, model: &str, event: &mut Input) -> Result<(), Fault> {
    let active = budget.active("turn");
    match event {
        Input::Session(setup) => configure(setup, model)?,
        Input::Native(native) if native.value["realtimeInput"].get("activityStart").is_some() => {
            budget.begin("turn")?
        }
        Input::Respond { .. } | Input::ToolResult(_) if !active => budget.begin("turn")?,
        Input::Audio { .. } | Input::Commit { .. } if !active => {
            return Err(Fault::new("send activityStart before realtime input")
                .with(ErrorCode("audio_policy".into()))
                .with(CloseCode(1008)));
        }
        _ => {}
    }
    Ok(())
}

/// What the Live API's setup keeps that the kind has no place for: each must
/// be an object if it is set.
const CONFIGURATION: &[&str] = &[
    "/setup/generationConfig",
    "/setup/realtimeInputConfig",
    "/setup/realtimeInputConfig/automaticActivityDetection",
    "/setup/proactivity",
];

/// The setup the budget allows: the configured `model`, capped output,
/// manual turns and no proactive audio.
fn configure(setup: &mut Session, model: &str) -> Result<(), Fault> {
    if setup.model.as_deref().is_some_and(|m| m != model) {
        return Err(Fault::bad_request(
            "model is outside the configured budget policy",
        ));
    }
    let native = setup
        .native
        .get_or_insert_with(|| NativeFields::new(GeminiLive::NAME, Default::default()));
    let mut kept = Value::Object(std::mem::take(&mut native.fields));
    if CONFIGURATION
        .iter()
        .any(|path| kept.pointer(path).is_some_and(|v| !v.is_object()))
    {
        return Err(Fault::bad_request("setup configuration must be an object"));
    }
    kept["setup"]["proactivity"]["proactiveAudio"] = false.into();
    if let Value::Object(kept) = kept {
        native.fields = kept;
    }
    setup.model = Some(model.to_owned());
    setup.max_output_tokens = u64::try_from(OUTPUT).ok();
    setup.turns = Some(Turns::Manual);
    Ok(())
}

/// What a turn's events said, until it ends.
#[derive(Default)]
struct Turn {
    used: Option<i64>,
    called: bool,
    interrupted: bool,
}

/// What `event` adds to its turn; at the turn's end, what it used and its
/// record. A response that calls tools goes on, in the same turn, once they
/// answer.
fn said(turn: &Mutex<Turn>, event: &Output) -> Option<(Option<i64>, Value)> {
    let mut turn = turn.lock();
    match event {
        Output::Usage(used) => {
            let tokens = used.tokens.input_tokens + used.tokens.output_tokens;
            let tokens = i64::try_from(tokens).unwrap_or(i64::MAX);
            turn.used = Some(turn.used.unwrap_or(0).saturating_add(tokens));
        }
        Output::SpeechStarted { .. } => turn.interrupted = true,
        Output::ToolCall { .. } => turn.called = true,
        Output::ResponseDone { .. } if turn.called => turn.called = false,
        Output::ResponseDone { status, .. } => {
            let Turn {
                used, interrupted, ..
            } = std::mem::take(&mut *turn);
            let interrupted = interrupted || *status == Status::Interrupted;
            let status = if interrupted {
                "interrupted"
            } else {
                "complete"
            };
            return Some((used, json!({"status": status, "usage": used})));
        }
        _ => {}
    }
    None
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

    fn active(&self, lane: &str) -> bool {
        self.lanes.lock().contains(lane)
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
