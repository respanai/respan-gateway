//! Billing: price each call's usage from your own price table, from
//! prepaid wallets that are held before a call and settled after it, with
//! a per-call cost cap.
//!
//! The kernel counts no usage. One plugin, `Billing`, works on the provider
//! edge, once per call, so a fallback's or a judge's calls are billed too,
//! each at the model it used:
//!
//! 1. before a call is sent, it bounds what the call can cost: its model's
//!    price, its input's size, and its answers' length, capped at `cap`
//!    output tokens each. A call with no bound (an unpriced model, non-text
//!    input) is refused. The bound is held on the payer's wallet, or the
//!    call is refused 402, so concurrent calls never spend past it;
//! 2. while the call runs, an `Output` filter sums the usage the provider
//!    reports, and estimates output from the text, cutting a stream that
//!    runs past the cap, for a provider that ignores `max_tokens`;
//! 3. when the call ends, it settles: the reported usage, priced; or, for
//!    a call cut off or never reported, at least its hold, as the provider
//!    may have billed it all. A call the provider refused costs nothing.
//!
//! **Order matters.** A filter sees a reply only if no filter before it
//! failed on it or held it back, and a call's output filters run the last
//! added first. Register `Billing` last, so its filter sees each reply
//! first and it prices the call as every other plugin left it.
//!
//! The wallets here are a map in memory. A shared store keys each hold by
//! an id and expires holds whose settlement was lost.
//!
//! Run it with `cargo run -p respan-gateway-examples --example billing`.
//! Look for the priced row, the unpriced model refused before any call,
//! the runaway stream cut and charged its hold, and fifty parallel requests
//! against a small wallet that never spend past it.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use mockllm::{Answer, FakeOpenAi};
use respan_gateway_conversation::{Block, Content, Input, Output, ResponseFormat, Usage};
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::{
    ErrorCode, Fault, Gateway, NativeValue, Plugin, Provider, ProviderContext, async_trait, futures,
};
use respan_gateway_http::http::{self, HeaderMap, HeaderValue, StatusCode};
use respan_gateway_http::http_body_util::BodyExt;
use respan_gateway_http::{Body, Credential};
use respan_gateway_http_faults::Status;
use respan_gateway_http_transport::HttpTransport;
use serde_json::{Map, Value, json};

/// Micro-dollars, or any unit you charge in, in the smallest step you count.
type Amount = i64;

/// The most output tokens one answer may have.
const CAP: u64 = 100;

#[tokio::main]
async fn main() -> Result<(), Fault> {
    let fake = FakeOpenAi::start().await;
    let wallets = Wallets::default()
        .open("acme", 1_000_000)
        .open("globex", 5_000);
    let rows = Rows::default();
    let billing = Billing {
        prices: HashMap::from([
            ("gpt-x".to_owned(), Price::new(2_000_000, 8_000_000)),
            ("gpt-x-mini".to_owned(), Price::new(150_000, 600_000)),
        ]),
        cap: CAP,
        wallets: wallets.clone(),
        rows: rows.clone(),
    };
    let gateway = gateway(&fake.base_url(), billing)?;

    println!("acme asks gpt-x and gpt-x-mini; each reports 40 in, 80 out");
    for model in ["gpt-x", "gpt-x-mini"] {
        fake.answer(reply(model, 40, 80));
        let status = ask(&gateway, "acme", model, false).await?.0;
        println!("  {model}: HTTP {status}");
    }

    println!("acme asks a model with no price");
    let (status, body) = ask(&gateway, "acme", "gpt-unpriced", false).await?;
    println!("  HTTP {status}: {body}");

    println!("acme streams from a provider that ignores max_tokens");
    fake.answer(runaway());
    let (status, body) = ask(&gateway, "acme", "gpt-x", true).await?;
    let last = body
        .lines()
        .rev()
        .find(|l| !l.is_empty())
        .unwrap_or_default();
    println!("  HTTP {status}, the stream ends: {last}");

    println!("globex sends fifty requests at once against its 5,000 wallet");
    let calls = (0..50).map(|_| {
        fake.answer(reply("gpt-x", 10, 100));
        let gateway = gateway.clone();
        tokio::spawn(async move { ask(&gateway, "globex", "gpt-x", false).await })
    });
    let served = futures::future::join_all(calls)
        .await
        .into_iter()
        .filter(|r| matches!(r, Ok(Ok((s, _))) if s.is_success()))
        .count();
    println!("  served {served} of 50; the rest got 402");

    println!("\nbilled calls:");
    for row in rows.0.lock().iter().filter(|r| r.account != "globex") {
        println!(
            "  {:6} {:10} in={:<4} out={:<4} held {:>4}, charged {:>4} ({})",
            row.account,
            row.model,
            row.usage.input_tokens,
            row.usage.output_tokens,
            row.held,
            row.charged,
            row.end
        );
    }
    for account in ["acme", "globex"] {
        let (limit, spent, held) = wallets.balance(account);
        println!("wallet {account}: spent {spent} of {limit}, {held} still held");
    }
    Ok(())
}

// Prices.

/// A model's price per million tokens, in micro-dollars. Add fields for
/// what else you price, such as cached input.
#[derive(Debug, Clone, Copy)]
struct Price {
    input: Amount,
    output: Amount,
}

impl Price {
    const fn new(input: Amount, output: Amount) -> Self {
        Self { input, output }
    }

    /// What `input` and `output` tokens cost, rounded up. Saturates rather
    /// than wraps, so an absurd count prices high, never low.
    fn cost(&self, input: u64, output: u64) -> Amount {
        let part = |n: u64, per: Amount| {
            Amount::try_from(n)
                .unwrap_or(Amount::MAX)
                .saturating_mul(per)
        };
        let per_million = part(input, self.input).saturating_add(part(output, self.output));
        per_million.saturating_add(999_999) / 1_000_000
    }

    fn of(&self, usage: &Usage) -> Amount {
        self.cost(usage.input_tokens, usage.output_tokens)
    }
}

// Wallets.

/// Prepaid wallets by account. Clones share them.
#[derive(Clone, Default)]
struct Wallets(Arc<Mutex<HashMap<String, Wallet>>>);

#[derive(Default)]
struct Wallet {
    limit: Amount,
    spent: Amount,
    held: Amount,
}

impl Wallets {
    fn open(self, account: &str, limit: Amount) -> Self {
        self.0.lock().entry(account.to_owned()).or_default().limit = limit;
        self
    }

    /// Hold `amount` on `account`, or refuse: 402 if it cannot cover it.
    fn hold(&self, account: &str, amount: Amount) -> Result<(), Fault> {
        let mut all = self.0.lock();
        let Some(wallet) = all.get_mut(account) else {
            return Err(refuse(
                StatusCode::UNAUTHORIZED,
                "unknown_account",
                "no such account".into(),
            ));
        };
        let available = wallet.limit - wallet.spent - wallet.held;
        if available < amount {
            let message = format!("'{account}' has {available} left; this call may cost {amount}");
            return Err(refuse(
                StatusCode::PAYMENT_REQUIRED,
                "insufficient_funds",
                message,
            ));
        }
        wallet.held += amount;
        Ok(())
    }

    /// Release a hold of `held` and charge `spent`, which may exceed it.
    fn settle(&self, account: &str, held: Amount, spent: Amount) {
        if let Some(wallet) = self.0.lock().get_mut(account) {
            wallet.held -= held;
            wallet.spent += spent;
        }
    }

    /// The limit, the spent and the held of `account`.
    fn balance(&self, account: &str) -> (Amount, Amount, Amount) {
        let all = self.0.lock();
        all.get(account)
            .map_or((0, 0, 0), |w| (w.limit, w.spent, w.held))
    }
}

// Usage rows.

/// One billed call.
#[derive(Debug, Clone)]
struct Row {
    account: String,
    model: String,
    usage: Usage,
    held: Amount,
    charged: Amount,
    /// `ok`, or the code of the fault that ended the call.
    end: String,
}

/// Rows in memory. In practice, an insert into your own table; keep it fast
/// or spawn it, as the request's end waits for its calls' end hooks.
#[derive(Clone, Default)]
struct Rows(Arc<Mutex<Vec<Row>>>);

// The plugin.

struct Billing {
    prices: HashMap<String, Price>,
    /// The most output tokens one answer may have.
    cap: u64,
    wallets: Wallets,
    rows: Rows,
}

/// One call's bill, in the call's extensions.
struct Call {
    account: String,
    model: String,
    price: Price,
    /// The input's bound, in tokens, for when no usage is reported.
    input: u64,
    /// The most output tokens of all its answers.
    output: u64,
    /// The most the call can cost, held on the wallet.
    hold: Amount,
    held: AtomicBool,
    usage: Mutex<Usage>,
    /// Output characters seen; about four a token.
    chars: AtomicUsize,
    cut: AtomicBool,
}

impl Call {
    fn seen_tokens(&self) -> u64 {
        self.chars.load(Ordering::Relaxed).div_ceil(4) as u64
    }
}

#[async_trait]
impl Plugin for Billing {
    async fn on_provider(&self, px: &mut ProviderContext) -> Result<(), Fault> {
        if !px.is_kind::<Input>() {
            return Ok(());
        }
        // Who pays: the client's key, as your auth plugin vouched for it.
        let credential = px.client().extensions().get::<Credential>();
        let account = credential.and_then(Credential::bearer).map(str::to_owned);
        let account =
            account.ok_or_else(|| refuse(StatusCode::UNAUTHORIZED, "no_key", "no key".into()))?;

        let chat = px.input::<Input>()?;
        let model = chat.model.clone().unwrap_or_default();
        let Some(&price) = self.prices.get(&model) else {
            let message = format!("no price for model '{model}'");
            return Err(refuse(StatusCode::BAD_REQUEST, "unpriced", message));
        };
        let limit = chat
            .options
            .max_output_tokens
            .map_or(self.cap, |n| n.min(self.cap));
        let input = input_bound(chat)?;
        let output = limit.saturating_mul(answers(chat)?);
        let call = Arc::new(Call {
            account,
            model,
            price,
            input,
            output,
            hold: price.cost(input, output),
            held: AtomicBool::new(false),
            usage: Mutex::default(),
            chars: AtomicUsize::new(0),
            cut: AtomicBool::new(false),
        });

        // Ask the provider for no more than the cap.
        px.filter(move |chat: &mut Input| {
            chat.options.max_output_tokens = Some(limit);
            Ok(())
        })?;
        let seen = call.clone();
        px.filter(move |reply: &mut Output| meter(&seen, reply))?;

        self.wallets.hold(&call.account, call.hold)?;
        call.held.store(true, Ordering::Relaxed);
        px.extensions_mut().insert(call);
        Ok(())
    }

    async fn on_provider_end(&self, px: &ProviderContext, end: &Result<(), Fault>) {
        let Some(call) = px.extensions().get::<Arc<Call>>().cloned() else {
            return;
        };
        if !call.held.swap(false, Ordering::Relaxed) {
            return;
        }
        let usage = *call.usage.lock();
        let cut = call.cut.load(Ordering::Relaxed);
        let charged = match end {
            // It never left the gateway.
            _ if !px.is_sent() => 0,
            // The provider answered with an error: its refusal.
            Err(_) if px.is_refused() && !cut => 0,
            Ok(()) if !usage.is_empty() => call.price.of(&usage),
            // Cut off, or never reported: the cost may be partial, so the
            // estimate, and never less than the hold.
            _ => {
                let input = Some(usage.input_tokens)
                    .filter(|&n| n > 0)
                    .unwrap_or(call.input);
                let output = usage.output_tokens.max(call.seen_tokens());
                call.price.cost(input, output).max(call.hold)
            }
        };
        self.wallets.settle(&call.account, call.hold, charged);
        self.rows.0.lock().push(Row {
            account: call.account.clone(),
            model: call.model.clone(),
            usage,
            held: call.hold,
            charged,
            end: end
                .as_ref()
                .map_or_else(|f| f.code().to_owned(), |()| "ok".into()),
        });
    }
}

/// Sum the usage `reply` reports into `call`, and count its output; fail
/// the call once its output runs past the cap.
fn meter(call: &Call, reply: &Output) -> Result<(), Fault> {
    // A delta counts its text or arguments, a tool call its name; a native
    // event its whole JSON, an overestimate, which errs toward cutting.
    let chars = match reply {
        Output::Usage(usage) => {
            *call.usage.lock() += usage;
            0
        }
        Output::Delta { delta, .. } => delta.as_str().len(),
        Output::BlockStart {
            kind: Block::ToolCall { name, .. },
            ..
        } => name.len(),
        Output::Native(NativeValue { value, .. }) => value.to_string().len(),
        _ => 0,
    };
    call.chars.fetch_add(chars, Ordering::Relaxed);
    if call.seen_tokens() <= call.output {
        return Ok(());
    }
    call.cut.store(true, Ordering::Relaxed);
    let message = "the answer ran past its cap".to_owned();
    Err(refuse(StatusCode::PAYMENT_REQUIRED, "cost_cap", message))
}

fn refuse(status: StatusCode, code: &'static str, message: String) -> Fault {
    Fault::new(message)
        .with(ErrorCode(code.into()))
        .with(Status(status))
}

// Bounds on what a call can cost, before it is sent.

/// The most input tokens `chat` can be. A token is at least a byte, so
/// bytes bound them: its text, tool calls and results, tools, the
/// structured-output schema, the JSON of what the kind has no place for,
/// and 8 a message for framing. Content with no byte bound, such as an
/// image, is refused.
fn input_bound(chat: &Input) -> Result<u64, Fault> {
    let json = |fields: &Map<String, Value>| serde_json::to_string(fields).map_or(0, |s| s.len());
    let mut bytes = chat.native.as_ref().map_or(0, |n| json(&n.fields));
    if let Some(ResponseFormat::JsonSchema {
        name,
        description,
        schema,
        ..
    }) = &chat.options.response_format
    {
        bytes += name.len() + description.as_deref().map_or(0, str::len) + schema.to_string().len();
    }
    for tool in &chat.tools {
        bytes += tool.name.len()
            + tool.description.as_deref().map_or(0, str::len)
            + tool.parameters.to_string().len()
            + tool.native.as_ref().map_or(0, |n| json(&n.fields));
    }
    for message in &chat.messages {
        bytes += content_bytes(&message.content)?
            + message.native.as_ref().map_or(0, |n| json(&n.fields))
            + 8;
    }
    Ok(bytes as u64)
}

fn content_bytes(parts: &[Content]) -> Result<usize, Fault> {
    let mut bytes = 0;
    for part in parts {
        bytes += match part {
            Content::Text { text, .. } => text.len(),
            Content::ToolCall {
                id,
                name,
                arguments,
                ..
            } => id.len() + name.len() + arguments.len(),
            Content::ToolResult { id, content, .. } => id.len() + content_bytes(content)?,
            _ => return Err(Fault::bad_request("no cost bound for non-text input")),
        };
    }
    Ok(bytes)
}

/// How many answers `chat` asks for: its `n`, 1 if unset.
fn answers(chat: &Input) -> Result<u64, Fault> {
    let n = chat.native.as_ref().and_then(|n| n.fields.get("n"));
    let n = n.map_or(Some(1), Value::as_u64);
    n.map(|n| n.max(1))
        .ok_or_else(|| Fault::bad_request("n must be a whole number"))
}

// The gateway, a scripted provider and a client.

fn gateway(base: &str, billing: Billing) -> Result<Gateway, Fault> {
    let mut headers = HeaderMap::new();
    headers.insert("authorization", HeaderValue::from_static("Bearer sk-test"));
    let transport = HttpTransport::new(base)?.with_headers(headers)?.connect();
    Gateway::new()
        .provider(Provider::new("openai", OpenAiChat, transport))?
        .client::<Input, _, _>(OpenAiChat)?
        // Last, so its filter sees each reply first.
        .plugin(billing)
        .target("openai")
}

/// A whole answer reporting `input` and `output` tokens.
fn reply(model: &str, input: u64, output: u64) -> Answer {
    Answer::body(json!({
        "id": "c1", "object": "chat.completion", "model": model,
        "choices": [{ "index": 0, "finish_reason": "stop",
            "message": { "role": "assistant", "content": "..." } }],
        "usage": { "prompt_tokens": input, "completion_tokens": output },
    }))
}

/// A stream from a provider that ignores `max_tokens`: 30 deltas of 20
/// characters, about 150 tokens, with its usage at the end.
fn runaway() -> Answer {
    let delta = json!({ "id": "c1", "model": "gpt-x",
        "choices": [{ "index": 0, "delta": { "content": "twenty characters.. " } }] });
    let usage = json!({ "id": "c1", "choices": [],
        "usage": { "prompt_tokens": 3, "completion_tokens": 150 } });
    let mut events = vec![format!("data: {delta}"); 30];
    events.extend([format!("data: {usage}"), "data: [DONE]".to_owned()]);
    Answer::events(events.into_iter().map(Value::String).collect())
}

/// `account` asks `model` for up to 5,000 tokens: the status, and the body
/// or how it broke off.
async fn ask(
    gateway: &Gateway,
    account: &str,
    model: &str,
    stream: bool,
) -> Result<(StatusCode, String), Fault> {
    let body = json!({ "model": model, "max_tokens": 5000, "stream": stream,
        "messages": [{ "role": "user", "content": "hi" }] });
    let mut req = http::Request::new(Body::full(body.to_string()));
    *req.method_mut() = http::Method::POST;
    *req.uri_mut() = http::Uri::from_static("/v1/chat/completions");
    let auth = HeaderValue::from_str(&format!("Bearer {account}"))
        .map_err(|e| respan_gateway_http_faults::built_wrong(e.into()))?;
    req.headers_mut().insert("authorization", auth);
    let (resp, _) = respan_gateway_http::handle(gateway, req, 32 << 20).await;
    let status = resp.status();
    let body = match resp.into_body().collect().await {
        Ok(body) => String::from_utf8_lossy(&body.to_bytes()).into_owned(),
        Err(fault) => format!("broken off: {fault}"),
    };
    // The call settles in its end hook; wait for it.
    gateway.idle().await;
    Ok((status, body))
}
