# Plugin guide

A plugin is how you add behavior to the gateway: edit requests, refuse
them, answer them yourself, reroute them, call models of your own, meter
usage, or rewrite responses as they stream. Routing, the tool loop,
pricing and Respan tracing are all plugins, built on the same public API
this guide covers.

Plugins are Rust, compiled in. There is no dynamic loading. You either
build your own gateway from the crates with your plugins registered, or
add your plugin to this repository's binary. Both are covered
[at the end](#running-your-plugin).

**Contents:**
[How a request flows](#how-a-request-flows) ·
[Setup](#setup) ·
[The four hooks](#the-four-hooks) ·
[Recipes](#recipes) ·
[Order](#order) ·
[Faults](#faults) ·
[Running your plugin](#running-your-plugin) ·
[Testing](#testing) ·
[Adding a plugin to the binary](#adding-a-plugin-to-the-binary) ·
[Further reading](#further-reading)

## How a request flows

```
 client ──bytes──▶ client format ──Input──▶ plugins ──▶ provider format ──bytes──▶ provider
   ▲               (decode)                 on_client   (encode)
   │                                        on_provider
   └──bytes──── client format ◀──Output─── filters ◀── provider format ◀──bytes──┘
                (encode)                                (decode)
```

A client's request is decoded by its **format** (OpenAI Chat, Anthropic
Messages, …) into a typed **`Input`**. Plugins see that value. A provider's
format encodes it again for the provider, and the provider's reply comes
back as a stream of typed **`Output`** events, which plugins can filter
before the client's format writes them out.

So a plugin never sees bytes, headers or keys, and never needs to know
which vendor is involved. A filter that edits a chat `Input` works the same
whether the client spoke OpenAI and the provider is Claude, or the other way
round.

Each **kind** of traffic has its own `Input`/`Output` pair, in its own crate:

| Kind | Crate | Types |
|---|---|---|
| Chat | `respan-gateway-conversation` | `Input` (model, messages, options, tools), `Output` (stream events) |
| Images, speech, transcription, embeddings, rerank, video | `respan-gateway-media` | `image::Input`, `speech::Input`, … with their `Output`s |
| Realtime sessions | `respan-gateway-realtime` | `Input` / `Output` session events, both ways |
| MCP | `respan-gateway-mcp` | `Input` / `Output` JSON-RPC envelopes |
| Typed decisions | `respan-gateway-decision` | `Input` / `Output` |

A chat reply is a sequence of `Output` events, the same for a stream and a
whole body: `Start`, then for each content block a `BlockStart`, its
`Delta`s (`Text`, `Arguments` or `Reasoning`) and a `BlockEnd`, with `Usage`
and `Finish` among them. Anything the kind has no form for travels as
`Native`, in its format's own JSON. The
[conversation kind's README](../crates/kinds/conversation/README.md) describes
the types in full.

## Setup

The crates are not on crates.io yet, so depend on them from git:

```toml
[dependencies]
respan-gateway-core = { git = "https://github.com/respanai/respan-gateway" }
respan-gateway-conversation = { git = "https://github.com/respanai/respan-gateway" }
```

A plugin that works on chat needs only these two. To host a gateway you also
need a format, a transport and a server
([Running your plugin](#running-your-plugin)).

## The four hooks

```rust
#[async_trait]
pub trait Plugin: Send + Sync + 'static {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>>;  // once per request
    async fn on_client_end(&self, cx: &ClientContext, end: &Result<()>);            // after it ends
    async fn on_provider(&self, px: &mut ProviderContext) -> Result<()>;            // before each call
    async fn on_provider_end(&self, px: &ProviderContext, end: &Result<()>);        // after each call
}
```

Every hook has a default that does nothing, so implement only the ones you
need.

- **`on_client`** runs once per request, before anything is sent. Return
  `Ok(None)` to let the request pass, `Ok(Some(response))` to answer it
  yourself (later plugins are skipped), or `Err(fault)` to refuse it. This
  is where you add filters, change the target, or make calls of your own.
- **`on_provider`** runs before every call to a provider. A request can make
  several calls: retries, fallbacks, a judge's call, each tool-loop step.
  `px.input()` is what this call sends, and `px.provider()` is where it goes.
  A limit reserves here.
- **`on_provider_end`** runs once per call when the call ends, fails, or is
  abandoned. A limit settles here.
- **`on_client_end`** runs once per request, after the response ended and
  every call's end hook ran. `end` is `Ok(())` if the client read the whole
  response; otherwise it is the fault that stopped it.

End hooks run detached, after the client already has its answer, so they
add no latency. `gateway.idle().await` waits for them.

**Every plugin sees all traffic.** A plugin for one kind checks it first
and lets the rest pass:

```rust
if !cx.is_kind::<Input>() {
    return Ok(None);
}
```

## Recipes

Every snippet below compiles against the current API. The imports they share:

```rust
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use respan_gateway_conversation::{Block, Delta, Input, Message, Output, Role, Usage};
use respan_gateway_core::futures::{StreamExt, stream};
use respan_gateway_core::{
    ClientContext, ErrorCode, Fault, Filter, Plugin, ProviderContext, Response, Result,
    async_trait,
};
```

### Edit the request

`cx.filter` with a closure over `&mut Input` edits the request before any
provider sees it. It works for every chat format, translated or not:

```rust
/// Every chat request at temperature 0.
pub struct ColdTemperature;

#[async_trait]
impl Plugin for ColdTemperature {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>> {
        if !cx.is_kind::<Input>() {
            return Ok(None); // Not chat: let it pass.
        }
        cx.filter(|chat: &mut Input| {
            chat.options.temperature = Some(0.0);
            Ok(())
        })?;
        Ok(None)
    }
}
```

The filter's type says what it edits. `Input` edits the request, and
`Output` edits the response.

### Refuse a request

Read the request with `cx.input()` and return a fault. The client gets an
error in its own format, with the status and code you give:

```rust
pub struct NoPreviews;

#[async_trait]
impl Plugin for NoPreviews {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>> {
        let Ok(chat) = cx.input::<Input>() else {
            return Ok(None);
        };
        if chat.model.as_deref().is_some_and(|m| m.contains("preview")) {
            return Err(Fault::bad_request("preview models are not allowed here")
                .with(ErrorCode("model_blocked".into())));
        }
        Ok(None)
    }
}
```

An OpenAI client receives:

```json
{"error":{"message":"preview models are not allowed here","type":"model_blocked","param":null,"code":"model_blocked"}}
```

### Rewrite the response as it streams

A filter that keeps state between events is a type that implements
`Filter`. `edit` takes each event and pushes what replaces it: the event
itself, an edited copy, several events, or none.

```rust
/// Masks a word in every text delta.
struct Mask(&'static str);

impl Filter<Output> for Mask {
    fn edit(&mut self, mut event: Output, out: &mut Vec<Output>) -> Result<()> {
        if let Output::Delta { delta: Delta::Text(text), .. } = &mut event {
            *text = text.replace(self.0, "****");
        }
        out.push(event);
        Ok(())
    }
}

pub struct Masking;

#[async_trait]
impl Plugin for Masking {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>> {
        if cx.is_kind::<Input>() {
            cx.filter(Mask("secret"))?;
        }
        Ok(None)
    }
}
```

Things to know about filters:

- **A word can be split across deltas** (`"sec"`, `"ret"`). A real redactor
  holds back text that might be the start of a match and releases it later.
  Override `Filter::end` to flush what it held when the stream ends. The
  [guardrail](../examples/guardrail.rs) example's `Holdback` does this, and
  the [PII](../examples/pii.rs) example holds back a split placeholder the
  same way.
- **Filters are synchronous.** For work that must wait, such as a remote
  classifier or a deadline, wrap the stream instead: `cx.wrap(|events:
  EventStream<Output>| ...)` replaces the stream with one of your own.
- **Every event reaches the filter**, keep-alives and usage included, so pass
  on immediately anything you have no reason to hold.
- `Err` from a filter fails the response. The client sees the fault, or, if
  the response is already streaming, the stream is cut.

### Answer the request yourself

Return a `Response` from `on_client` and no provider is called. This is how
a cache, a mock or a canned reply works. The client's format encodes your
events, as a stream or a whole body as the client asked:

```rust
pub struct Canned;

#[async_trait]
impl Plugin for Canned {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>> {
        let Ok(chat) = cx.input::<Input>() else {
            return Ok(None);
        };
        if chat.model.as_deref() != Some("ping") {
            return Ok(None);
        }
        let reply = [
            Output::start("canned-1", "ping"),
            Output::block_start(0, Block::Text),
            Output::text(0, "pong"),
            Output::block_end(0),
        ];
        Ok(Some(Response::stream(stream::iter(reply.map(Ok)))))
    }
}
```

`Response::stream` takes any stream of `Result<Output>`, so you can also
merge or relay the outputs of calls you made yourself.

### Call a model yourself, and reroute

`cx.call(provider)` makes a call with the request as it currently stands.
`.with_input(...)` sends an input of your own instead, such as a judge's
prompt. Read the answer with `.outputs()`. `cx.set_target` then chooses
where the client's request goes:

```rust
pub struct Judge;

#[async_trait]
impl Plugin for Judge {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>> {
        let Ok(chat) = cx.input::<Input>() else {
            return Ok(None);
        };
        let question = format!(
            "Answer yes or no: is this about cooking?\n\n{}",
            chat.messages.last().map(Message::text).unwrap_or_default()
        );
        let mut ask = Input::new([Message::new(Role::User, question)]);
        ask.model = Some("gpt-6-luna".into());
        let mut verdict = String::new();
        let mut outputs = cx.call("openai").with_input(ask).await?.outputs::<Output>();
        while let Some(event) = outputs.next().await {
            if let Output::Delta { delta: Delta::Text(text), .. } = event? {
                verdict.push_str(&text);
            }
        }
        if verdict.to_lowercase().starts_with("yes") {
            cx.set_target("anthropic");
        }
        Ok(None)
    }
}
```

Your own calls pass through every plugin's `on_provider`, so they are
metered, logged and guarded like any other call. To race several calls,
hold each with `resp.buffer_until(..)` and reply with the first. The
`Plugin` trait's documentation shows the pattern.

### Meter usage on every call

A provider-edge filter sees each call's events before any client-edge
filter, including calls other plugins make. `Output::Usage` carries the
token counts the provider reports:

```rust
#[derive(Clone, Default)]
pub struct Meter(Arc<Mutex<Usage>>);

#[async_trait]
impl Plugin for Meter {
    async fn on_provider(&self, px: &mut ProviderContext) -> Result<()> {
        if !px.is_kind::<Input>() {
            return Ok(());
        }
        let total = self.0.clone();
        px.filter(move |event: &mut Output| {
            if let Output::Usage(usage) = event {
                *total.lock() += &*usage;
            }
            Ok(())
        })
    }
}
```

Because the plugin is `Clone` and shares its state, the host keeps one copy
to read the totals from.

### Keep state across a request's hooks

Each request carries typed **extensions**. Put a value in during
`on_client` and read it back in the end hook:

```rust
#[derive(Clone)]
struct Started(Instant);

pub struct Timing;

#[async_trait]
impl Plugin for Timing {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>> {
        cx.extensions_mut().insert(Started(Instant::now()));
        Ok(None)
    }

    async fn on_client_end(&self, cx: &ClientContext, end: &Result<()>) {
        if let Some(Started(at)) = cx.extensions().get::<Started>() {
            let outcome = match end {
                Ok(()) => "ok",
                Err(fault) => fault.code(),
            };
            println!("{} {outcome} in {:?}", cx.format(), at.elapsed());
        }
    }
}
```

A call has extensions of its own: `cx.call(..).with_extension(mark)` puts a
value where that call's `on_provider` and `on_provider_end` can read it,
through `px.extensions()`.

## Order

Plugins run in the order they are registered, and **order is behavior**:

- `on_client` runs first to last. The first plugin that answers or refuses
  stops the rest, so register a guard that must see everything **before**
  any plugin that answers, such as a cache.
- **Input filters** apply first-added first, so a plugin sees the request
  after earlier plugins edited it. Register a check of the final request
  **last**.
- **Output filters** apply last-added first: an earlier plugin sees the
  response after later plugins edited it, like layers of middleware. On a
  call, the provider-edge filters run before the client-edge ones.
- `on_provider` runs for the gateway's plugins in order, then for the
  plugins registered on that one provider (`Provider::new(..).plugin(..)`).
- `on_provider_end` runs before `on_client_end`.

## Faults

`Fault` is the one error type. Its constructor says whose fault it is, and
the client's format turns it into a status and an error body:

| Constructor | Whose fault | HTTP status |
|---|---|---|
| `Fault::bad_request(msg)` | The client asked for something wrong | 400 |
| `Fault::not_found(msg)` | Nothing serves what the client asked for | 404 |
| `Fault::upstream(msg)` | A provider failed | 502 |
| `Fault::config(msg)` | The gateway is misconfigured | 500 |
| `Fault::internal(msg)` | A bug | 500 |

Add typed extensions with `.with(..)`: `ErrorCode("your_code".into())` sets
the error code clients see, `RetryAfter` sets a retry hint, and
`respan_gateway_http_faults::Status(StatusCode::FORBIDDEN)` sets the HTTP
status. A panic in a hook or filter becomes an `internal` fault for that
request alone. It does not take down the gateway.

## Running your plugin

### In a gateway of your own

A `Gateway` is built by value from providers, client formats and plugins,
and served with `respan-gateway-serve`. A minimal gateway that speaks OpenAI
Chat and Anthropic Messages to clients and sends everything to OpenAI:

```rust
use respan_gateway_conversation::Input;
use respan_gateway_conversation_http::anthropic::AnthropicMessages;
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::{Gateway, Provider, Result};
use respan_gateway_http::http::{HeaderMap, HeaderValue};
use respan_gateway_http_transport::HttpTransport;

fn gateway(api_key: &str) -> Result<Gateway> {
    let mut headers = HeaderMap::new();
    let bearer = HeaderValue::from_str(&format!("Bearer {api_key}"))
        .map_err(respan_gateway_core::Fault::from_error)?;
    headers.insert("authorization", bearer);
    let openai = HttpTransport::new("https://api.openai.com/v1")?
        .with_headers(headers)?
        .connect();
    Gateway::new()
        .provider(Provider::new("openai", OpenAiChat, openai))?
        .client::<Input, _, _>(OpenAiChat)?
        .client::<Input, _, _>(AnthropicMessages)?
        .plugin(NoPreviews)
        .plugin(ColdTemperature)
        .plugin(Masking)
        .target("openai")
}

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let gateway = gateway(&std::env::var("OPENAI_API_KEY")?)?;
    let app = respan_gateway_serve::router(gateway.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8080").await?;
    axum::serve(listener, app).await?;
    gateway.idle().await; // Let end hooks finish.
    Ok(())
}
```

Add more providers, in any format, and route between them with
`cx.set_target`, or register the first-party `Routing` plugin
(`respan-gateway-plugin-routing`). The vendor kit (`crates/vendors/kit`) can
build a provider from a vendor's name and key, as the binary does. The
[embedded example](../examples/embedded.rs) runs a gateway in process
with no HTTP server at all.

### In this repository's binary

See [Adding a plugin to the binary](#adding-a-plugin-to-the-binary).

## Testing

Test against [`mockllm`](https://github.com/lihe07/mockllm), a scripted fake
of the OpenAI, Anthropic and Gemini APIs, over real HTTP. No keys and no
charges:

```rust
#[tokio::test]
async fn requests_reach_the_provider_cold() {
    let fake = mockllm::FakeOpenAi::start().await;
    fake.answer(mockllm::Answer::text("the secret is out"));
    let gateway = gateway_at(&fake.base_url()).unwrap(); // As above, at the fake's URL.

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/chat/completions", listener.local_addr().unwrap());
    let app = respan_gateway_serve::router(gateway.clone());
    tokio::spawn(async move { axum::serve(listener, app).await });

    let reply = reqwest::Client::new()
        .post(url)
        .header("content-type", "application/json")
        .body(r#"{"model":"gpt-6-luna","temperature":1.2,"messages":[{"role":"user","content":"hi"}]}"#)
        .send().await.unwrap()
        .text().await.unwrap();

    assert!(reply.contains("****"));                               // Masking
    assert_eq!(fake.requests()[0].body["temperature"], 0.0);       // ColdTemperature
}
```

Then cover what makes a plugin hard to get right: streamed and whole
responses, a client of another format (send to `/v1/messages` too), a
provider's error, the client leaving mid-stream (`on_client_end` gets
`Abandoned`), and the order relative to the plugins you expect it beside.

## Adding a plugin to the binary

First-party plugins live in `crates/plugins/<name>`, one crate each. To add
one:

1. **Create the crate.** Name it `respan-gateway-plugin-<name>`, and copy the
   `[package]` fields and `[lints]` from a sibling such as
   `crates/plugins/respan/Cargo.toml`. The workspace already includes
   `crates/plugins/*`. Depend only on core, the kinds you read, and what the
   plugin itself needs. A plugin should not depend on `apps/gateway`.
2. **Register the dependency.** Add it to `[workspace.dependencies]` in the
   root `Cargo.toml` and to `apps/gateway/Cargo.toml`.
3. **Give it config.** Add a field to `PluginsConfig` in
   `apps/gateway/src/config.rs`, as `Option<YourConfig>` so it is off unless
   configured. Mark the struct `#[serde(deny_unknown_fields)]`, and read
   secrets through `env:` like the other plugins (`compose::secret`). Keep
   secrets out of `Debug`.
4. **Register it.** Add it in `with_plugins` in
   `apps/gateway/src/app/built.rs`. The binary fixes the order whatever the
   file says, and the comments there explain why each plugin sits where it
   does. Roughly: plugins that observe everything (tracing, the request log)
   come first; plugins that route or edit the request come next; plugins that
   answer the request themselves (the tool loop, then `Dispatch`, which runs
   a route's fallbacks) come after those; and pricing comes last.
5. **Document it.** Add its settings to
   [`docs/configuration.md`](configuration.md), a row to
   [`crates/plugins/README.md`](../crates/plugins/README.md), and a `README.md`
   in the crate.
6. **Test it.** Unit-test the plugin in its crate with a small `Gateway` and
   `mockllm`, and add a case to `apps/gateway/src/compose/tests.rs` that
   configures it from TOML.

## Further reading

- The first-party plugins, smallest first:
  [`credentials`](../crates/plugins/credentials),
  [`relay`](../crates/plugins/relay),
  [`pricing`](../crates/plugins/pricing),
  [`respan`](../crates/plugins/respan),
  [`routing`](../crates/plugins/routing),
  [`tool-loop`](../crates/plugins/tool-loop).
- [`examples/`](../examples/README.md), runnable: guardrails, PII
  redaction across streams, billing and wallets, admission control, durable
  jobs, realtime policies.
- [`crates/core/README.md`](../crates/core/README.md) and the rustdoc on
  `Plugin`, `ClientContext`, `ProviderContext`, `Call` and `Response`, the
  contract in full.
