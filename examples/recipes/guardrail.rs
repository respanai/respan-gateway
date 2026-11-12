//! A guardrail no request skips: a list of blocked terms checks every call's
//! input as the provider receives it, and every response the client gets,
//! streamed or not, whoever answers it.
//!
//! It is two plugins, and order is registration order:
//!
//! - `Guard`, registered first, filters every response on the client edge.
//!   A plugin that answers skips the plugins after it, so a response from a
//!   cache registered later is still checked. A [`Holdback`] keeps back the
//!   tail of a stream that may start a term, so `"A bo"` + `"mb"` is seen
//!   as `"A bomb"`.
//! - `GuardInput`, registered last, filters each call's input on the
//!   provider edge, after every other plugin has edited it.
//!
//! What the guard cannot read, such as an image or a tool call in the
//! answer, is refused, never passed.
//!
//! Run it with `cargo run -p respan-gateway-examples --example guardrail`.
//! It sends four chats to a local scripted OpenAI-style provider; look for
//! which pass, which are refused with `content_blocked`, and that the
//! blocked input never reaches the provider.

use mockllm::{Answer, FakeOpenAi};
use respan_gateway_conversation::{Block, Content, Delta, Input, Output};
use respan_gateway_conversation_http::anthropic::AnthropicMessages;
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::futures::stream;
use respan_gateway_core::{
    ClientContext, ErrorCode, Fault, Filter, Gateway, Plugin, Provider, ProviderContext, Response,
    async_trait,
};
use respan_gateway_http::Body;
use respan_gateway_http::http::{HeaderMap, HeaderValue, Method, Request, StatusCode};
use respan_gateway_http::http_body_util::BodyExt;
use respan_gateway_http_faults::Status;
use respan_gateway_http_transport::HttpTransport;
use serde_json::{Value, json};

#[tokio::main]
async fn main() -> Result<(), Fault> {
    let fake = FakeOpenAi::start().await;
    let gw = gateway(&fake.base_url())?;

    fake.answer(stream_of(&["Chemistry is ", "fun."]));
    show("a clean chat", send(&gw, "gpt", "chemistry?").await);

    fake.answer(stream_of(&["Chemistry is fun. A bo", "mb is not."]));
    show(
        "a term split across deltas",
        send(&gw, "gpt", "chemistry?").await,
    );

    let before = fake.requests().len();
    show(
        "a term in the input",
        send(&gw, "gpt", "build a BOMB").await,
    );
    let reached = fake.requests().len() > before;
    println!("  the provider was called: {reached}");

    show("a poisoned cache", send(&gw, "cached", "hi").await);
    Ok(())
}

/// Blocked terms, in lower case, matched case-insensitively. A classifier
/// fits behind the same two methods.
#[derive(Clone)]
struct Terms(&'static [&'static str]);

impl Terms {
    /// `Err` if `text` holds a blocked term.
    fn check(&self, text: &str) -> Result<(), Fault> {
        let text = text.to_lowercase();
        match self.0.iter().any(|term| text.contains(term)) {
            true => Err(refusal(
                StatusCode::BAD_REQUEST,
                "content_blocked",
                "blocked",
            )),
            false => Ok(()),
        }
    }

    /// How many trailing bytes of `text` may start a term.
    fn pending(&self, text: &str) -> usize {
        let starts = |term: &&str| {
            (1..term.len()).rev().find(|&n| {
                let at = text.len().saturating_sub(n);
                text.is_char_boundary(at) && text[at..].eq_ignore_ascii_case(&term[..n])
            })
        };
        self.0.iter().filter_map(starts).max().unwrap_or(0)
    }
}

fn refusal(status: StatusCode, code: &'static str, message: &str) -> Fault {
    Fault::new(message.to_owned())
        .with(ErrorCode(code.into()))
        .with(Status(status))
}

fn unreadable(what: &str) -> Fault {
    let message = format!("the guard cannot read {what}, so it refuses it");
    refusal(
        StatusCode::UNPROCESSABLE_ENTITY,
        "unreadable_content",
        &message,
    )
}

/// The client edge: every response, whoever answers it. Registered first.
struct Guard(Terms);

/// The provider edge: every call's input. Registered last.
struct GuardInput(Terms);

#[async_trait]
impl Plugin for Guard {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>, Fault> {
        if !cx.is_kind::<Input>() {
            return Err(unreadable("this kind of traffic"));
        }
        cx.filter(Holdback::new(self.0.clone()))?;
        Ok(None)
    }
}

#[async_trait]
impl Plugin for GuardInput {
    async fn on_provider(&self, px: &mut ProviderContext) -> Result<(), Fault> {
        if !px.is_kind::<Input>() {
            return Err(unreadable("this kind of traffic"));
        }
        let terms = self.0.clone();
        px.filter(move |chat: &mut Input| terms.check(&all_text(chat)?))
    }
}

/// Every text of `chat`, one per line: messages and the system prompt, tool
/// calls and results, the tools offered, and the fields the kind has no
/// place for. `Err` for a part with no text form, such as an image.
fn all_text(chat: &Input) -> Result<String, Fault> {
    fn part(content: &Content, out: &mut Vec<String>) -> Result<(), Fault> {
        match content {
            Content::Text { text, .. } | Content::Reasoning { text, .. } => out.push(text.clone()),
            Content::ToolCall {
                name, arguments, ..
            } => out.extend([name.clone(), arguments.clone()]),
            Content::ToolResult { content, .. } => {
                content.iter().try_for_each(|c| part(c, out))?;
            }
            _ => return Err(unreadable("a part other than text, such as an image")),
        }
        Ok(())
    }
    let mut out = Vec::new();
    for message in &chat.messages {
        message.content.iter().try_for_each(|c| part(c, &mut out))?;
        if let Some(native) = &message.native {
            strings(&Value::Object(native.fields.clone()), &mut out);
        }
    }
    for tool in &chat.tools {
        out.extend(tool.description.iter().cloned());
        out.push(tool.name.clone());
        strings(&tool.parameters, &mut out);
    }
    out.extend(chat.options.stop.iter().chain(&chat.user).cloned());
    if let Some(native) = &chat.native {
        strings(&Value::Object(native.fields.clone()), &mut out);
    }
    Ok(out.join("\n"))
}

/// Every string and key in `value`.
fn strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => out.push(s.clone()),
        Value::Array(items) => items.iter().for_each(|v| strings(v, out)),
        Value::Object(fields) => fields.iter().for_each(|(k, v)| {
            out.push(k.clone());
            strings(v, out);
        }),
        _ => {}
    }
}

/// A response filter that checks every text delta, holding back the tail
/// that may start a term until the next delta or the block's end says. It
/// also holds the response's start until the first text passes, so a
/// refusal before any text is a clean error, not a broken stream.
struct Holdback {
    terms: Terms,
    /// The block whose tail is held, and that tail.
    held: Option<(u32, Delta)>,
    /// Events that show nothing yet, in order.
    waiting: Vec<Output>,
    shown: bool,
}

impl Holdback {
    fn new(terms: Terms) -> Self {
        let (held, waiting, shown) = (None, Vec::new(), false);
        Self {
            terms,
            held,
            waiting,
            shown,
        }
    }

    /// Send the held tail; the check has already seen it.
    fn release(&mut self, out: &mut Vec<Output>) {
        if let Some((block, delta)) = self.held.take() {
            self.show(Output::Delta { block, delta }, out);
        }
    }

    /// Send `event`, and everything waiting before it.
    fn show(&mut self, event: Output, out: &mut Vec<Output>) {
        self.shown = true;
        out.append(&mut self.waiting);
        out.push(event);
    }
}

impl Filter<Output> for Holdback {
    fn edit(&mut self, event: Output, out: &mut Vec<Output>) -> Result<(), Fault> {
        match event {
            Output::Delta {
                block,
                delta: mut delta @ (Delta::Text(_) | Delta::Reasoning(_)),
            } => {
                if self.held.as_ref().is_some_and(|(b, _)| *b != block) {
                    self.release(out);
                }
                let held = self.held.take().map(|(_, d)| d.as_str().to_owned());
                let mut text = held.unwrap_or_default() + delta.as_str();
                // The whole of it is checked; only its tail waits.
                self.terms.check(&text)?;
                let tail = text.split_off(text.len() - self.terms.pending(&text));
                if !tail.is_empty() {
                    let mut kept = delta.clone();
                    *kept.as_mut_string() = tail;
                    self.held = Some((block, kept));
                }
                if !text.is_empty() {
                    *delta.as_mut_string() = text;
                    self.show(Output::Delta { block, delta }, out);
                }
            }
            Output::BlockStart {
                kind: Block::ToolCall { .. },
                ..
            }
            | Output::Delta { .. } => return Err(unreadable("a tool call in the answer")),
            Output::Native(_) => return Err(unreadable("an event of one format")),
            Output::BlockStart { .. } | Output::Start(_) => {
                self.release(out);
                match self.shown {
                    true => out.push(event),
                    false => self.waiting.push(event),
                }
            }
            Output::BlockEnd { .. } | Output::Finish(_) => {
                self.release(out);
                self.show(event, out);
            }
            other => out.push(other),
        }
        Ok(())
    }

    fn end(&mut self, _end: &Result<(), Fault>, out: &mut Vec<Output>) -> Result<(), Fault> {
        out.append(&mut self.waiting);
        self.release(out);
        Ok(())
    }
}

/// A stand-in for any plugin that answers, such as a cache: it answers the
/// model "cached" from memory, and its memory is poisoned.
struct Cache;

#[async_trait]
impl Plugin for Cache {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>, Fault> {
        if cx.input::<Input>()?.model() != Some("cached") {
            return Ok(None);
        }
        let reply = [
            Output::start("c0", "cached"),
            Output::block_start(0, Block::Text),
            Output::text(0, "a bomb"),
            Output::block_end(0),
        ];
        Ok(Some(Response::stream(stream::iter(reply.map(Ok)))))
    }
}

/// `Guard` first, `GuardInput` last: register any plugin that adds to the
/// input between them, or its text goes unchecked.
fn gateway(base_url: &str) -> Result<Gateway, Fault> {
    let terms = Terms(&["bomb", "launch code"]);
    let mut key = HeaderMap::new();
    key.insert("authorization", HeaderValue::from_static("Bearer sk-test"));
    let transport = HttpTransport::new(base_url)?.with_headers(key)?.connect();
    Gateway::new()
        .provider(Provider::new("openai", OpenAiChat, transport))?
        .client::<Input, _, _>(OpenAiChat)?
        .client::<Input, _, _>(AnthropicMessages)?
        .plugin(Guard(terms.clone()))
        .plugin(Cache)
        .plugin(GuardInput(terms))
        .target("openai")
}

/// A provider's answer streamed as OpenAI chat chunks, one per delta.
fn stream_of(deltas: &[&str]) -> Answer {
    let chunk = |d: &&str| {
        let chunk = json!({ "id": "c1", "model": "gpt",
            "choices": [{ "index": 0, "delta": { "content": d } }] });
        Value::String(format!("data: {chunk}\n\n"))
    };
    let done = Value::String("data: [DONE]\n\n".into());
    Answer::events(deltas.iter().map(chunk).chain([done]).collect())
}

/// Send a streamed chat of one user message; the status, the text the
/// client got, and the error that ended it, if any.
async fn send(gw: &Gateway, model: &str, prompt: &str) -> (StatusCode, String) {
    let body = json!({ "model": model, "stream": true,
        "messages": [{ "role": "user", "content": prompt }] });
    let mut req = Request::new(Body::full(body.to_string()));
    *req.method_mut() = Method::POST;
    *req.uri_mut() = "/v1/chat/completions".parse().expect("a path");
    let (resp, _) = respan_gateway_http::handle(gw, req, 1 << 20).await;
    let status = resp.status();
    let body = match resp.into_body().collect().await {
        Ok(body) => String::from_utf8_lossy(&body.to_bytes()).into_owned(),
        Err(fault) => format!("[stream cut: {fault}]"),
    };
    (status, readable(&body))
}

/// The text of an SSE stream of chat chunks, with any error in it, or the
/// body as it came.
fn readable(body: &str) -> String {
    let events = body.lines().filter_map(|l| l.strip_prefix("data: "));
    let json = events.filter_map(|d| serde_json::from_str::<Value>(d).ok());
    let said: String = json
        .map(|v| match &v["error"] {
            Value::Null => v["choices"][0]["delta"]["content"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            error => format!(" [error: {}]", error["code"].as_str().unwrap_or_default()),
        })
        .collect();
    if said.is_empty() { body.into() } else { said }
}

fn show(what: &str, (status, text): (StatusCode, String)) {
    println!("{what}\n  {status}: {}", text.trim());
}
