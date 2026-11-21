//! A model in this process, reached as a provider.
//!
//! The `Local` format only dials: its I/O value is the model itself, so no
//! bytes are written or read. It takes the call's [`Input`] and answers with
//! [`Output`]s, which any client of the conversation kind (OpenAI Chat,
//! Anthropic Messages) gets in its own protocol, JSON or SSE as it asked.
//! It is a full provider, so it can be a target or a fallback, and every
//! provider-edge plugin (billing, limits, guards) sees its calls.
//!
//! Run it with `cargo run -p respan-gateway-examples --example custom_transport`.
//! The toy model echoes the last message. Look for the same answer served as
//! one OpenAI JSON body, then as an SSE stream, a word per chunk, with usage.

use std::sync::Arc;

use respan_gateway_conversation::{Block, Finish, Input, Output, Usage};
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::futures::channel::oneshot;
use respan_gateway_core::futures::{FutureExt, sink, stream};
use respan_gateway_core::{Connection, EventSink, EventStream, Fault, Format, Gateway, Provider};
use respan_gateway_http::Body;
use respan_gateway_http::http;
use respan_gateway_http::http_body_util::BodyExt;

/// The most bytes of one message the gateway holds.
const LIMIT: usize = 32 << 20;

#[tokio::main]
async fn main() -> Result<(), Fault> {
    let model: Model = Arc::new(echo);
    let gw = Gateway::new()
        .provider(Provider::new("local", Local, move |_| model.clone()))?
        .client::<Input, _, _>(OpenAiChat)?
        .target("local")?;

    let whole = r#"{"model":"echo","messages":[{"role":"user","content":"hello there"}]}"#;
    let streamed = r#"{"model":"echo","stream":true,"stream_options":{"include_usage":true},"messages":[{"role":"user","content":"hello there"}]}"#;
    for (name, body) in [("Whole", whole), ("Streamed", streamed)] {
        let mut req = http::Request::new(Body::full(body));
        *req.method_mut() = http::Method::POST;
        *req.uri_mut() = http::Uri::from_static("/v1/chat/completions");
        let (resp, _) = respan_gateway_http::handle(&gw, req, LIMIT).await;
        let status = resp.status();
        let body = resp.into_body().collect().await?.to_bytes();
        println!(
            "{name}: {status}\n{}\n",
            String::from_utf8_lossy(&body).trim_end()
        );
    }
    Ok(())
}

/// Your model: a conversation in, its answer out.
type Model = Arc<dyn Fn(&Input) -> Result<String, Fault> + Send + Sync>;

/// A toy model that echoes the last message.
fn echo(chat: &Input) -> Result<String, Fault> {
    let last = chat.messages.last().map(|m| m.text()).unwrap_or_default();
    Ok(format!("you said: {last}"))
}

/// A model in this process: each provider attempt is handed the model.
struct Local;

impl Format<Model, Output, Input> for Local {
    fn name(&self) -> &str {
        "local"
    }

    fn bind(&self, model: Model) -> Result<Connection<Output, Input>, Fault> {
        // The answer's stream, sent once the call's input arrives.
        let (send, receive) = oneshot::channel();
        let incoming = async move { receive.await.map_err(|_| Fault::abandoned()) };

        let outgoing = EventSink::preparing(move |flow: &mut EventStream<Input>| {
            let chat = flow
                .value()
                .transpose()?
                .ok_or_else(|| Fault::bad_request("no conversation"))?;
            let answer = model(chat)?;
            let replies = reply(chat, &answer);
            let outputs = EventStream::stream(stream::iter(replies.into_iter().map(Ok)));
            Ok(sink::unfold(
                Some((send, outputs)),
                |state, item: Result<Input, Fault>| async move {
                    item?;
                    if let Some((send, outputs)) = state {
                        let _ = send.send(outputs);
                    }
                    Ok::<_, Fault>(None)
                },
            ))
        });
        Ok((incoming.boxed(), outgoing))
    }
}

/// `answer` as the conversation kind's events: a start, one text delta per
/// word, then usage and the finish. A word counts as a token here.
fn reply(chat: &Input, answer: &str) -> Vec<Output> {
    let words = |s: &str| s.split_whitespace().count() as u64;
    let mut usage = Usage::default();
    usage.input_tokens += chat.messages.iter().map(|m| words(&m.text())).sum::<u64>();
    usage.output_tokens += words(answer);
    let model = chat.model.clone().unwrap_or_else(|| "local".into());

    let mut replies = vec![
        Output::start("local-1", model),
        Output::block_start(0, Block::Text),
    ];
    // Each word keeps the space after it: "you ", "said: ", ...
    replies.extend(answer.split_inclusive(' ').map(|w| Output::text(0, w)));
    replies.push(Output::block_end(0));
    replies.push(Output::usage(usage));
    replies.push(Output::finished(Finish::Stop));
    replies
}
