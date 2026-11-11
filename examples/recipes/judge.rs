//! A plugin that calls a model to pick the model for a request.
//!
//! The client asks for `auto`. The `Judge` plugin sends the last message to a
//! small model with "HARD or EASY?", then targets the strong provider or the
//! cheap one and rewrites the model name to match.
//!
//! The judge's call is a subrequest (`cx.call("judge").with_input(prompt)`),
//! so it is a call like any other: it inherits the request's extensions, and
//! every plugin's `on_provider` runs before it (egress rules, keys, budgets).
//! `cx.call` runs provider hooks only, so the judge does not judge its own
//! call.
//!
//! Run it with `cargo run -p respan-gateway-examples --example judge`.
//! The first request is judged HARD and answered by `strong-large`; the second
//! carries `x-blocked`, and the egress plugin refuses the judge's own call with
//! a 403. Look for the calls each provider got and the provider edge's log.

use std::sync::Arc;

use mockllm::{Answer, FakeOpenAi};
use parking_lot::Mutex;
use respan_gateway_conversation::{Assembled, Input, Message, Output, Role};
use respan_gateway_conversation_http::StreamResponse;
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::futures::TryStreamExt;
use respan_gateway_core::{
    ClientContext, ErrorCode, Fault, Gateway, Plugin, Provider, ProviderContext, Response,
    async_trait,
};
use respan_gateway_http::Body;
use respan_gateway_http::http::{self, HeaderMap, HeaderValue, StatusCode};
use respan_gateway_http::http_body_util::BodyExt;
use respan_gateway_http::raw::Original;
use respan_gateway_http_faults::Status;
use respan_gateway_http_transport::HttpTransport;

/// The most bytes of one message the gateway holds.
const LIMIT: usize = 32 << 20;

#[tokio::main]
async fn main() -> Result<(), Fault> {
    let (judge, cheap, strong) = tokio::join!(
        FakeOpenAi::start(),
        FakeOpenAi::start(),
        FakeOpenAi::start()
    );
    judge.answer(answer("HARD"));
    strong.answer(answer("Working on it."));

    let egress = Egress::default();
    let gw = Gateway::new()
        .provider(Provider::new(
            "judge",
            OpenAiChat,
            transport(&judge)?.connect(),
        ))?
        .provider(Provider::new(
            "cheap",
            OpenAiChat,
            transport(&cheap)?.connect(),
        ))?
        .provider(Provider::new(
            "strong",
            OpenAiChat,
            transport(&strong)?.connect(),
        ))?
        .client::<Input, _, _>(OpenAiChat)?
        .plugin(egress.clone())
        .plugin(Judge)
        .target("cheap")?;

    let body =
        r#"{"model":"auto","messages":[{"role":"user","content":"Prove the Riemann hypothesis"}]}"#;
    println!("A hard question:");
    send(&gw, body, false).await?;
    for (name, fake) in [("judge", &judge), ("cheap", &cheap), ("strong", &strong)] {
        for sent in fake.requests() {
            println!("  {name} got: {}", String::from_utf8_lossy(&sent.raw));
        }
    }
    println!("  provider edge saw: {:?}\n", egress.0.lock());

    println!("A blocked client:");
    egress.0.lock().clear();
    send(&gw, body, true).await?;
    println!("  provider edge saw: {:?}", egress.0.lock());
    Ok(())
}

/// The models the tiers serve, in place of the client's `auto`.
const STRONG_MODEL: &str = "strong-large";
const CHEAP_MODEL: &str = "cheap-small";

/// Routes each conversation by a small model's verdict on its last message.
struct Judge;

#[async_trait]
impl Plugin for Judge {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>, Fault> {
        let Ok(chat) = cx.input::<Input>() else {
            return Ok(None);
        };
        let last = chat.messages.last().map(Message::text).unwrap_or_default();
        let mut prompt = Input::new([
            Message::new(
                Role::System,
                "Output HARD or EASY: how hard is this request?",
            ),
            Message::new(Role::User, last),
        ]);
        prompt.model = Some("judge-mini".into());
        prompt.options.max_output_tokens = Some(1);

        let replies: Vec<Output> = cx
            .call("judge")
            .with_input(prompt)
            .with_extension(StreamResponse(false))
            .await?
            .outputs()
            .try_collect()
            .await?;
        let mut verdict = Assembled::default();
        replies.iter().try_for_each(|reply| verdict.push(reply))?;

        let (target, model) = match verdict.text().contains("HARD") {
            true => ("strong", STRONG_MODEL),
            false => ("cheap", CHEAP_MODEL),
        };
        cx.set_target(target);
        cx.filter(move |chat: &mut Input| {
            chat.model = Some(model.into());
            Ok(())
        })?;
        Ok(None)
    }
}

/// A provider-edge plugin: it records every call and refuses calls for a
/// client that sent `x-blocked`, the judge's call included.
#[derive(Clone, Default)]
struct Egress(Arc<Mutex<Vec<String>>>);

#[async_trait]
impl Plugin for Egress {
    async fn on_provider(&self, px: &mut ProviderContext) -> Result<(), Fault> {
        self.0.lock().push(px.provider().to_owned());
        let asked = px.extensions().get::<Original>().and_then(Original::get);
        if asked.is_some_and(|a| a.headers().contains_key("x-blocked")) {
            return Err(Fault::new("no calls for you")
                .with(ErrorCode("egress".into()))
                .with(Status(StatusCode::FORBIDDEN)));
        }
        Ok(())
    }
}

/// Posts a chat `body` to the gateway, `x-blocked` if `blocked`, and prints
/// the answer.
async fn send(gw: &Gateway, body: &'static str, blocked: bool) -> Result<(), Fault> {
    let mut req = http::Request::new(Body::full(body));
    *req.method_mut() = http::Method::POST;
    *req.uri_mut() = http::Uri::from_static("/v1/chat/completions");
    if blocked {
        req.headers_mut()
            .insert("x-blocked", HeaderValue::from_static("1"));
    }
    let (resp, _) = respan_gateway_http::handle(gw, req, LIMIT).await;
    let status = resp.status();
    let body = resp.into_body().collect().await?.to_bytes();
    gw.idle().await;
    println!("  client got: {status} {}", String::from_utf8_lossy(&body));
    Ok(())
}

/// A transport to a fake OpenAI, with its own key.
fn transport(fake: &FakeOpenAi) -> Result<HttpTransport, Fault> {
    let mut headers = HeaderMap::new();
    headers.insert("authorization", HeaderValue::from_static("Bearer sk-test"));
    HttpTransport::new(fake.base_url())?.with_headers(headers)
}

/// A whole chat completion saying `text`.
fn answer(text: &str) -> Answer {
    let body = serde_json::json!({"id": "c", "choices": [{"index": 0, "finish_reason": "stop",
        "message": {"role": "assistant", "content": text}}],
        "usage": {"prompt_tokens": 9, "completion_tokens": 1}});
    Answer::raw(body.to_string())
}
