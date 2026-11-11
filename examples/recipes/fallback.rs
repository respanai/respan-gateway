//! Retries, fallback, hedging and stall watchdogs, written as routes.
//!
//! The router's rules decide where a model name goes:
//! - `gpt-6-luna` tries OpenAI twice, waiting as its `Retry-After` asks, then
//!   falls back to Claude, a provider of another format sent its own model name.
//! - `gpt-6-luna-fast` races two OpenAI deployments, the second started 200 ms
//!   after the first; the first to reach content wins.
//!
//! A `watchdog` around the OpenAI transport fails a try whose head takes over
//! 2 s or whose stream goes silent for 1 s. A stream that fails after its first
//! content reached the client is never retried: it is committed.
//!
//! Run it with `cargo run -p respan-gateway-examples --example fallback`.
//! Each scenario prints its attempts (provider, model, index, role, outcome),
//! what the client got, and how many requests each fake provider saw.

use std::sync::Arc;
use std::time::Duration;

use mockllm::{Answer, FakeAnthropic, FakeOpenAi};
use respan_gateway_conversation::{Input, Output};
use respan_gateway_conversation_http::anthropic::AnthropicMessages;
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::{Fault, Gateway, Plugin, Provider, ProviderContext, async_trait};
use respan_gateway_http::Body;
use respan_gateway_http::call::watchdog;
use respan_gateway_http::http::{self, HeaderMap, HeaderValue};
use respan_gateway_http::http_body_util::BodyExt;
use respan_gateway_http_faults::retryable;
use respan_gateway_http_transport::HttpTransport;
use respan_gateway_plugin_fallback::{Attempt, Backoff, Until};
use respan_gateway_plugin_routing::{Candidate, Dispatch, Model, Routes, Routing, Rule, Speaks};

/// The most bytes of one message the gateway holds.
const LIMIT: usize = 32 << 20;

#[tokio::main]
async fn main() -> Result<(), Fault> {
    let streamed = |model: &str| {
        format!(
            r#"{{"model":"{model}","stream":true,"messages":[{{"role":"user","content":"hi"}}]}}"#
        )
    };

    run(
        "OpenAI answers 429 with Retry-After: 1, twice; Claude answers",
        Duration::ZERO,
        streamed("gpt-6-luna"),
        vec![
            (Fake::OpenAi, rate_limited()),
            (Fake::OpenAi, rate_limited()),
        ],
    )
    .await?;

    run(
        "OpenAI's stream breaks after its first words: committed, not retried",
        Duration::ZERO,
        streamed("gpt-6-luna"),
        vec![(Fake::OpenAi, openai_stream("Hello").cut_after(1))],
    )
    .await?;

    run(
        "OpenAI never answers: the watchdog fails each try after 2 s; Claude answers",
        Duration::from_secs(3600),
        streamed("gpt-6-luna"),
        vec![],
    )
    .await?;

    let essay = r#"{"id":"c","model":"gpt-6-luna","choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":"an essay"}}]}"#;
    run(
        "OpenAI's whole answer takes 1.5 s: within the head bound, not cut",
        Duration::from_millis(1500),
        r#"{"model":"gpt-6-luna","messages":[{"role":"user","content":"write an essay"}]}"#.into(),
        vec![(Fake::OpenAi, Answer::raw(essay))],
    )
    .await?;

    run(
        "Hedged: OpenAI takes 900 ms; openai-eu starts 200 ms in and wins",
        Duration::from_millis(900),
        streamed("gpt-6-luna-fast"),
        vec![
            (Fake::OpenAi, openai_stream("late")),
            (Fake::Europe, openai_stream("fast")),
        ],
    )
    .await?;
    Ok(())
}

/// The routes. Each answer is judged at its first content; `Until::End`
/// would hold each whole before choosing.
fn routes() -> Routes {
    let to = |candidates: &[&str]| candidates.iter().map(|c| Candidate::parse(c)).collect();

    let mut fallback = Rule::new("gpt-6-luna", to(&["openai", "anthropic/claude-haiku-5-5"]));
    fallback.backoff = Backoff::new(2, Duration::from_millis(250), Duration::from_secs(5));
    fallback.until = Until::content(Output::is_content);

    let mut hedge = Rule::new(
        "gpt-6-luna-fast",
        to(&["openai/gpt-6-luna", "openai-eu/gpt-6-luna"]),
    );
    hedge.hedge = Some(Duration::from_millis(200));
    hedge.until = Until::content(Output::is_content);

    let speaks = |format: &str, internal: &str| {
        vec![Speaks {
            format: format.into(),
            kind: "chat",
            internal: internal.into(),
        }]
    };
    Routes::new()
        .provider("openai", speaks("openai-chat", "openai"))
        .provider("openai-eu", speaks("openai-chat", "openai-eu"))
        .provider("anthropic", speaks("anthropic-messages", "anthropic"))
        .rule(fallback)
        .rule(hedge)
}

/// Prints each attempt as it ends. Only the attempt whose response reached
/// the client is the `Winner`.
struct Log;

#[async_trait]
impl Plugin for Log {
    async fn on_provider_end(&self, px: &ProviderContext, end: &Result<(), Fault>) {
        let Some(attempt) = px.extensions().get::<Attempt>() else {
            return;
        };
        let end = end.as_ref().err().map_or("ok", Fault::code);
        let model = px.input::<Input>().ok().and_then(|c| c.model.as_deref());
        println!(
            "  attempt: {} {} #{} {:?} {end}",
            px.provider(),
            model.unwrap_or("-"),
            attempt.index(),
            attempt.role(),
        );
    }
}

#[derive(Clone, Copy)]
enum Fake {
    OpenAi,
    Europe,
}

/// Sends `body` to a fresh gateway whose OpenAI answers after `delay`, each
/// fake provider answering with its `replies` in order.
async fn run(
    name: &str,
    delay: Duration,
    body: String,
    replies: Vec<(Fake, Answer)>,
) -> Result<(), Fault> {
    println!("{name}");
    let (openai, europe, claude) = tokio::join!(
        FakeOpenAi::start(),
        FakeOpenAi::start(),
        FakeAnthropic::start()
    );
    for (fake, answer) in replies {
        match fake {
            Fake::OpenAi => openai.answer(answer),
            Fake::Europe => europe.answer(answer),
        }
    }
    claude.answer(claude_stream());

    // A whole answer's head comes only once it is done: a deployment waits
    // minutes for it, and seconds between stream events.
    let backend = transport(&openai.base_url(), "authorization", "Bearer sk-test")?;
    let primary = watchdog(
        move |request| {
            let backend = backend.clone();
            async move {
                tokio::time::sleep(delay).await;
                backend.send(request).await
            }
        },
        Duration::from_secs(2),
        Duration::from_secs(1),
    );

    let gw = Gateway::new()
        .client::<Input, _, _>(OpenAiChat)?
        .provider(Provider::new(
            "openai",
            OpenAiChat,
            respan_gateway_http::connect(primary, LIMIT),
        ))?
        .provider(Provider::new(
            "openai-eu",
            OpenAiChat,
            transport(&europe.base_url(), "authorization", "Bearer sk-test")?.connect(),
        ))?
        .provider(Provider::new(
            "anthropic",
            AnthropicMessages,
            transport(&claude.base_url(), "x-api-key", "sk-ant-test")?.connect(),
        ))?
        .plugin(Log)
        .plugin(Routing::new(Arc::new(routes())).format(
            "openai-chat",
            "chat",
            Model::of(Input::model, Input::set_model),
        ))
        .plugin(Dispatch::new(retryable));

    let mut req = http::Request::new(Body::full(body));
    *req.method_mut() = http::Method::POST;
    *req.uri_mut() = http::Uri::from_static("/v1/chat/completions");
    let (resp, _) = respan_gateway_http::handle(&gw, req, LIMIT).await;
    let status = resp.status();
    let body = resp.into_body().collect().await?.to_bytes();
    gw.idle().await;

    let body = String::from_utf8_lossy(&body);
    let last = body.trim_end().lines().last().unwrap_or_default();
    println!("  client: {status}, last line: {last}");
    println!(
        "  requests: openai {}, openai-eu {}, anthropic {}\n",
        openai.requests().len(),
        europe.requests().len(),
        claude.requests().len()
    );
    Ok(())
}

/// An HTTP transport to `base` that sends one auth header.
fn transport(base: &str, header: &'static str, key: &'static str) -> Result<HttpTransport, Fault> {
    let mut headers = HeaderMap::new();
    headers.insert(header, HeaderValue::from_static(key));
    HttpTransport::new(base)?.with_headers(headers)
}

/// Each of `data` as one SSE `data:` event.
fn sse(data: &[&str]) -> Answer {
    let events = data
        .iter()
        .map(|d| serde_json::Value::String(format!("data: {d}")))
        .collect();
    Answer::events(events)
}

/// An OpenAI chat stream saying `text`, then its usage.
fn openai_stream(text: &str) -> Answer {
    let say = format!(r#"{{"choices":[{{"index":0,"delta":{{"content":"{text}"}}}}]}}"#);
    let usage = r#"{"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":2}}"#;
    sse(&[&say, usage, "[DONE]"])
}

/// An Anthropic Messages stream saying "Hi from Claude".
fn claude_stream() -> Answer {
    sse(&[
        r#"{"type":"message_start","message":{"id":"m","type":"message","role":"assistant","model":"claude-haiku-5-5","content":[],"usage":{"input_tokens":5,"output_tokens":1}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi from Claude"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#,
        r#"{"type":"message_stop"}"#,
    ])
}

/// Too many requests; try again in a second.
fn rate_limited() -> Answer {
    Answer::error(429)
        .message("rate limit exceeded")
        .retry_after(1)
}
