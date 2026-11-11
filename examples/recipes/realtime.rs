//! A two-way OpenAI Realtime session through the gateway, metered and capped
//! by plugins over the realtime kind's events.
//!
//! - Every turn is held on the client's wallet before the provider sees it:
//!   the hold bounds what the turn reads (everything the session has said
//!   and heard so far, audio by its duration) plus the most it may say.
//!   A `response.create` is rewritten to ask for at most `TURN` output
//!   tokens, so the hold is a true bound. A turn the wallet cannot cover is
//!   refused, closing the client with 1008.
//! - Each turn is settled when its `response.done` passes, at the text and
//!   audio usage it reports, so a long session is billed as it goes. A turn
//!   cut before its end (the client hung up) is charged its whole hold: a cut
//!   is never billed as $0.
//! - A session is cut at `SESSION_MAX` with a normal close (1000,
//!   `max_length`). The cut wraps the provider's events, so the relay and
//!   the provider's own handshake stay.
//!
//! It runs offline against a local scripted OpenAI Realtime provider:
//!
//! ```bash
//! cargo run -p respan-gateway-examples --example realtime
//! ```
//!
//! Look for the provider reading the capped `max_output_tokens`, each turn's
//! text and audio usage with the wallet's spending after it, the abandoned
//! turn charged its hold, the small wallet's refusal, and the idle session's
//! `max_length` close.
//!
//! For real use, the provider is `wss://api.openai.com`, with each client's
//! own key relayed (as here) or the gateway's own key on the transport
//! (`WsTransport::with_headers`).
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use mockllm::{Answer, FakeOpenAi};
use parking_lot::Mutex;
use respan_gateway_conversation::Content;
use respan_gateway_core::futures::{SinkExt, Stream, StreamExt, stream};
use respan_gateway_core::{
    ClientContext, ErrorCode, EventStream, Fault, Filter, Gateway, NativeFields, Plugin, Provider,
    Response, async_trait,
};
use respan_gateway_http::Credential;
use respan_gateway_realtime::{Audio, Input, Output, Status, Usage};
use respan_gateway_realtime_ws::OpenAiRealtime;
use respan_gateway_ws::{CloseCode, WsTransport};
use serde_json::{Map, Value, json};
use tokio_tungstenite::tungstenite::Message;

/// The most output tokens a turn may say. A plan would set it per tenant.
const TURN: i64 = 1_000;
/// What a frame costs besides its content: its type, ids and framing.
const FRAME: i64 = 128;
/// The longest a session may last; short here, so the narration shows it.
const SESSION_MAX: Duration = Duration::from_millis(1_500);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The provider answers each `response.create` with the next of these.
    let openai = FakeOpenAi::start().await;
    openai.answer(Answer::text("Noted, the invoice goes out on Friday."));
    openai.answer(Answer::audio(vec![0; 9_600]));
    openai.answer(Answer::text("Once upon a time there was a very long story indeed.").pace(100));
    let provider = openai.base_url().replace("http://", "ws://");

    let books = Books::default();
    books.open("sk-demo", 100_000);
    books.open("sk-small", 500);
    let log = Log::default();
    let gw = gateway(&provider, &books, &log)?;
    let addr = listen(&gw).await?;

    println!("1. A text turn");
    let mut ws = connect(addr, "sk-demo").await?;
    hear(&mut ws).await?; // session.created
    say(&mut ws, user_says("Remind me when the invoice goes out.")).await?;
    let text = json!({ "type": "response.create", "response": { "output_modalities": ["text"] } });
    say(&mut ws, text).await?;
    let events = turn(&mut ws).await?;
    let requests = openai.requests();
    let asked = requests
        .iter()
        .rev()
        .find(|r| r.method == "WS" && r.body["type"] == "response.create");
    let cap = asked.map(|r| r.body["response"]["max_output_tokens"].clone());
    println!(
        "   provider was asked for at most {} tokens",
        cap.unwrap_or_default()
    );
    println!(
        "   client heard: {:?}",
        deltas(&events, "response.output_text.delta")
    );
    log.print(&books);

    println!("2. An audio turn, in the same session");
    let half_second = base64_zeros(24_000);
    let append = json!({ "type": "input_audio_buffer.append", "audio": half_second });
    say(&mut ws, append).await?;
    say(&mut ws, json!({ "type": "input_audio_buffer.commit" })).await?;
    say(&mut ws, json!({ "type": "response.create" })).await?;
    let events = turn(&mut ws).await?;
    let audio = events
        .iter()
        .filter(|e| e["type"] == "response.output_audio.delta");
    println!("   client heard {} chunks of audio", audio.count());
    ws.close(None).await?;
    gw.idle().await;
    log.print(&books);

    println!("3. The client hangs up mid-turn");
    let mut ws = connect(addr, "sk-demo").await?;
    hear(&mut ws).await?;
    say(&mut ws, user_says("Tell me a long story.")).await?;
    say(&mut ws, json!({ "type": "response.create" })).await?;
    while hear(&mut ws).await?["type"] != "response.output_audio_transcript.delta" {}
    drop(ws);
    gw.idle().await;
    log.print(&books);

    println!("4. A wallet that cannot cover a turn");
    let mut ws = connect(addr, "sk-small").await?;
    hear(&mut ws).await?;
    say(&mut ws, json!({ "type": "response.create" })).await?;
    println!("   client: {}", closed(&mut ws).await);
    gw.idle().await;
    log.print(&books);

    println!("5. A session left open past its maximum length");
    let mut ws = connect(addr, "sk-demo").await?;
    hear(&mut ws).await?;
    println!("   client: {}", closed(&mut ws).await);
    gw.idle().await;
    log.print(&books);
    Ok(())
}

/// The gateway: OpenAI Realtime sessions relayed to `provider`, each held
/// and settled on the wallet in `books` its key names, and cut at
/// `SESSION_MAX`.
fn gateway(provider: &str, books: &Books, log: &Log) -> Result<Gateway, Fault> {
    let transport = WsTransport::new(provider)?;
    let gw = Gateway::new()
        .client::<Input, _, _>(OpenAiRealtime)?
        .provider(Provider::new("openai", OpenAiRealtime, transport.connect()))?
        .target("openai")?;
    Ok(gw
        .plugin(respan_gateway_plugin_relay::Relay)
        .plugin(Wallets {
            books: books.clone(),
            log: log.clone(),
        })
        .plugin(MaxLength(SESSION_MAX)))
}

/// The plugin: each session gets a tab on its client's wallet.
struct Wallets {
    books: Books,
    log: Log,
}

#[async_trait]
impl Plugin for Wallets {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>, Fault> {
        let key = cx.extensions().get::<Credential>().and_then(account);
        let key = key.ok_or_else(|| {
            Fault::new("no key")
                .with(ErrorCode("no_key".into()))
                .with(CloseCode(1008))
        })?;
        let tab = Arc::new(Mutex::new(Tab {
            books: self.books.clone(),
            log: self.log.clone(),
            key,
            context: 0,
            holds: VecDeque::new(),
            begun: 0,
            usage: Usage::default(),
        }));
        let asks = tab.clone();
        cx.filter(move |input: &mut Input| asks.lock().before(input))?;
        cx.filter(Answers(tab.clone()))?;
        cx.extensions_mut().insert(tab);
        Ok(None)
    }

    async fn on_client_end(&self, cx: &ClientContext, end: &Result<(), Fault>) {
        if let Some(tab) = cx.extensions().get::<Arc<Mutex<Tab>>>() {
            tab.lock().end();
        }
        let code = end.as_ref().err().map_or("ok", Fault::code);
        self.log.0.lock().push(format!("session ended: {code}"));
    }
}

/// The account a credential names: the token of a bearer, of a key in the
/// query (`api_key=sk-..`) or of a subprotocol
/// (`openai-insecure-api-key.sk-..`). A real one looks the key up.
fn account(credential: &Credential) -> Option<String> {
    let values = credential.headers().iter();
    let values = values.filter_map(|(_, value)| value.to_str().ok());
    let value = values.chain(credential.query()).next()?;
    let token = value.rsplit([' ', '.', '=']).next();
    Some(token.unwrap_or(value).to_owned())
}

/// One session's turns on its wallet.
struct Tab {
    books: Books,
    log: Log,
    key: String,
    /// A bound on the tokens of all the session said and heard: every turn
    /// reads it whole, so it only grows.
    context: i64,
    /// What each turn not yet done holds, oldest first.
    holds: VecDeque<i64>,
    /// How many of those the provider has begun.
    begun: usize,
    /// What the provider reported for the turn in progress.
    usage: Usage,
}

impl Tab {
    /// Count what the client says; cap and hold a `response.create`.
    fn before(&mut self, input: &mut Input) -> Result<(), Fault> {
        self.context = self.context.saturating_add(input_bound(input));
        match input {
            Input::Session(session) => {
                // The cap of the turns the provider starts on its own.
                let turn = TURN.unsigned_abs();
                if session.max_output_tokens.is_none_or(|n| n > turn) {
                    session.max_output_tokens = Some(turn);
                }
            }
            Input::Respond { native } => {
                let ours = || NativeFields::new(OpenAiRealtime::NAME, Map::new());
                let fields = &mut native.get_or_insert_with(ours).fields;
                let response = fields.entry("response").or_insert_with(|| json!({}));
                if !response.is_object() {
                    *response = json!({});
                }
                let cap = &mut response["max_output_tokens"];
                if cap.as_i64().is_none_or(|n| !(1..=TURN).contains(&n)) {
                    *cap = TURN.into();
                }
                self.hold()?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Hold a turn's worst case, or refuse it, closing the client.
    fn hold(&mut self) -> Result<(), Fault> {
        let amount = self.context.saturating_add(TURN);
        self.books.reserve(&self.key, amount)?;
        self.holds.push_back(amount);
        Ok(())
    }

    /// Count what the provider says. A turn it begins on its own (server
    /// voice detection) is held as it begins; a `response.done` settles the
    /// oldest begun turn at its usage.
    fn after(&mut self, output: &Output) -> Result<(), Fault> {
        self.context = self.context.saturating_add(output_bound(output));
        match output {
            Output::ResponseStart { .. } => {
                if self.begun == self.holds.len() {
                    self.hold()?;
                }
                self.begun += 1;
            }
            Output::Usage(usage) => self.usage += usage,
            Output::ResponseDone { status, .. } if self.begun > 0 => {
                let held = self.holds.pop_front().unwrap_or_default();
                self.begun -= 1;
                let usage = std::mem::take(&mut self.usage);
                let spent = usage.tokens.input_tokens + usage.tokens.output_tokens;
                let spent = i64::try_from(spent).unwrap_or(i64::MAX);
                self.books.settle(&self.key, held, spent);
                self.log.0.lock().push(format!(
                    "turn {}: text {} in / {} out, audio {} in / {} out; charged {spent}",
                    finish(status),
                    usage
                        .tokens
                        .input_tokens
                        .saturating_sub(usage.input_audio_tokens),
                    usage
                        .tokens
                        .output_tokens
                        .saturating_sub(usage.output_audio_tokens),
                    usage.input_audio_tokens,
                    usage.output_audio_tokens,
                ));
            }
            _ => {}
        }
        Ok(())
    }

    /// Turns cut before their `response.done` may have cost all they were
    /// held for: each is charged that.
    fn end(&mut self) {
        for held in std::mem::take(&mut self.holds) {
            self.books.settle(&self.key, held, held);
            let line = format!("turn cut: charged its hold, {held}");
            self.log.0.lock().push(line);
        }
        self.begun = 0;
    }
}

/// The provider's side of a tab.
struct Answers(Arc<Mutex<Tab>>);

impl Filter<Output> for Answers {
    fn edit(&mut self, output: Output, out: &mut Vec<Output>) -> Result<(), Fault> {
        self.0.lock().after(&output)?;
        out.push(output);
        Ok(())
    }
}

/// A response's end as the log names it.
fn finish(status: &Status) -> &str {
    match status {
        Status::Completed => "completed",
        Status::Cancelled | Status::Interrupted => "cancelled",
        Status::Length | Status::Filtered => "incomplete",
        Status::Native { reason, .. } => reason,
        _ => "failed",
    }
}

/// A bound on the tokens of `audio`: OpenAI bills at most one per 50ms.
fn audio_tokens(audio: &Audio) -> i64 {
    let ms = audio.duration().as_millis().div_ceil(50);
    i64::try_from(ms).unwrap_or(i64::MAX)
}

/// A bound on the tokens of `text`: a token of text is at least a byte.
fn text_tokens(text: &str) -> i64 {
    i64::try_from(text.len()).unwrap_or(i64::MAX)
}

/// What an input adds to the context.
fn input_bound(input: &Input) -> i64 {
    let content = match input {
        Input::Session(session) => {
            let tools = session.tools.iter().flatten();
            let tools =
                tools.map(|t| text_tokens(&t.name) + text_tokens(&t.parameters.to_string()));
            session.instructions.as_deref().map_or(0, text_tokens) + tools.sum::<i64>()
        }
        Input::Message { message, .. } => message
            .content
            .iter()
            .map(|part| match part {
                Content::Text { text, .. } => text_tokens(text),
                Content::Native(part) => text_tokens(&part.value.to_string()),
                _ => 0,
            })
            .sum(),
        Input::Audio { audio, .. } => audio_tokens(audio),
        Input::ToolResult(result) => text_tokens(&result.output),
        Input::Native(event) => text_tokens(&event.value.to_string()),
        _ => 0,
    };
    content.saturating_add(FRAME)
}

/// What an output adds to the context.
fn output_bound(output: &Output) -> i64 {
    let content = match output {
        Output::Text { text, .. }
        | Output::Transcript { text, .. }
        | Output::InputTranscript { text, .. } => text_tokens(text),
        Output::Audio { audio, .. } => audio_tokens(audio),
        Output::ToolCall { arguments, .. } => text_tokens(arguments),
        _ => 0,
    };
    content.saturating_add(FRAME)
}

/// Caps a session's length: its events are cut at a deadline chosen in
/// `on_client`, so a plugin could choose it per session (a tenant's plan, a
/// drain signal). The client gets Close 1000 with the reason `max_length`.
struct MaxLength(Duration);

#[async_trait]
impl Plugin for MaxLength {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>, Fault> {
        let limit = self.0;
        cx.wrap(move |events| until(limit, events))?;
        Ok(None)
    }
}

/// `events` until `limit` has passed, then a fault that closes normally.
fn until(
    limit: Duration,
    events: EventStream<Output>,
) -> impl Stream<Item = Result<Output, Fault>> {
    let state = Some((events, Box::pin(tokio::time::sleep(limit))));
    stream::unfold(state, |state| async {
        let (mut events, mut timer) = state?;
        tokio::select! {
            () = &mut timer => {
                let over = Fault::new("the session reached its maximum length")
                    .with(ErrorCode("max_length".into()))
                    .with(CloseCode(1000));
                Some((Err(over), None))
            }
            next = events.next() => Some((next?, Some((events, timer)))),
        }
    })
}

/// Wallets in memory: what each may spend, has spent and holds for turns in
/// flight. Not durable billing; clones share them.
#[derive(Clone, Default)]
struct Books(Arc<Mutex<HashMap<String, Account>>>);

#[derive(Default)]
struct Account {
    limit: i64,
    spent: i64,
    held: i64,
}

impl Books {
    fn open(&self, key: &str, limit: i64) {
        self.0.lock().entry(key.to_owned()).or_default().limit = limit;
    }

    /// Hold `amount` on `key`'s wallet, or refuse with 1008.
    fn reserve(&self, key: &str, amount: i64) -> Result<(), Fault> {
        let mut books = self.0.lock();
        let account = books
            .get_mut(key)
            .filter(|a| a.limit - a.spent - a.held >= amount);
        let account = account.ok_or_else(|| {
            Fault::new(format!("wallet '{key}' cannot cover this turn"))
                .with(ErrorCode("insufficient_funds".into()))
                .with(CloseCode(1008))
        })?;
        account.held += amount;
        Ok(())
    }

    /// Release a hold of `held` and charge `spent`.
    fn settle(&self, key: &str, held: i64, spent: i64) {
        if let Some(account) = self.0.lock().get_mut(key) {
            account.held -= held;
            account.spent += spent;
        }
    }

    fn spent(&self, key: &str) -> i64 {
        self.0.lock().get(key).map_or(0, |a| a.spent)
    }
}

/// What the plugins recorded, for the narration.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<String>>>);

impl Log {
    /// Print what was recorded since the last call, and the demo wallet.
    fn print(&self, books: &Books) {
        let lines = std::mem::take(&mut *self.0.lock());
        if lines.is_empty() {
            return;
        }
        for line in lines {
            println!("   {line}");
        }
        println!("   sk-demo has spent {}", books.spent("sk-demo"));
    }
}

/// Serve `gateway` to WebSocket clients on a free port.
async fn listen(gateway: &Gateway) -> std::io::Result<std::net::SocketAddr> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let app = respan_gateway_serve::Host::new(gateway.clone())
        .limit(|_| 32 << 20)
        .router();
    tokio::spawn(async move { axum::serve(listener, app).await });
    Ok(addr)
}

type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A Realtime client of the gateway at `addr`, with `key`.
async fn connect(
    addr: std::net::SocketAddr,
    key: &str,
) -> Result<Client, Box<dyn std::error::Error>> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = format!("ws://{addr}/v1/realtime?model=gpt-realtime").into_client_request()?;
    req.headers_mut()
        .insert("authorization", format!("Bearer {key}").parse()?);
    Ok(tokio_tungstenite::connect_async(req).await?.0)
}

async fn say(ws: &mut Client, event: Value) -> Result<(), Box<dyn std::error::Error>> {
    ws.send(Message::Text(event.to_string().into())).await?;
    Ok(())
}

/// The next event the client hears, or how its session closed.
async fn hear(ws: &mut Client) -> Result<Value, String> {
    let next = tokio::time::timeout(Duration::from_secs(5), ws.next()).await;
    match next.map_err(|_| "no event in time".to_owned())? {
        Some(Ok(Message::Text(text))) => serde_json::from_str(&text).map_err(|e| e.to_string()),
        Some(Ok(Message::Close(Some(close)))) => Err(format!(
            "closed {} ({})",
            u16::from(close.code),
            close.reason
        )),
        _ => Err("closed".to_owned()),
    }
}

/// How the session closed, past any events before.
async fn closed(ws: &mut Client) -> String {
    loop {
        if let Err(closed) = hear(ws).await {
            return closed;
        }
    }
}

/// The events up to and including a `response.done`.
async fn turn(ws: &mut Client) -> Result<Vec<Value>, String> {
    let mut events = Vec::new();
    loop {
        let event = hear(ws).await?;
        let done = event["type"] == "response.done";
        events.push(event);
        if done {
            return Ok(events);
        }
    }
}

/// The text of `events`' deltas of type `kind`.
fn deltas(events: &[Value], kind: &str) -> String {
    let deltas = events.iter().filter(|e| e["type"] == kind);
    deltas.filter_map(|e| e["delta"].as_str()).collect()
}

/// The user says `text`.
fn user_says(text: &str) -> Value {
    json!({ "type": "conversation.item.create", "item": { "type": "message", "role": "user",
        "content": [{ "type": "input_text", "text": text }] } })
}

/// `bytes` of silence, as base64.
fn base64_zeros(bytes: usize) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(vec![0_u8; bytes])
}
