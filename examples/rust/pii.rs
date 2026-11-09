//! PII kept from the model: e-mail addresses are hidden behind placeholders
//! such as `<EMAIL_1>` in every call's input as the provider receives it,
//! and restored in the reply the client gets, a placeholder split across
//! stream deltas or inside a tool call's arguments included.
//!
//! It is two plugins, and order is registration order:
//!
//! - `Pii`, registered first, keeps one vault per request and restores
//!   every response on the client edge, whoever answers. A [`Restore`]
//!   filter holds `<EMA` back until `IL_1>` says it is a placeholder.
//! - `PiiInput`, registered last, redacts each call's input on the provider
//!   edge, after every other plugin has added to it. Register any plugin
//!   that adds to the input before it, or its text goes out unredacted.
//!
//! What it cannot read, such as an image, is refused, as it might hold PII.
//!
//! Run it with `cargo run -p respan-gateway-examples --example pii`. It
//! sends two chats to a local scripted OpenAI-style provider; compare what
//! the provider was sent with what the client got.

use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::sync::Arc;

use mockllm::{Answer, FakeOpenAi};
use respan_gateway_conversation::{Block, Content, Delta, Input, Output};
use respan_gateway_conversation_http::openai::OpenAiChat;
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

    // The model answers with the placeholders it saw, split across deltas.
    fake.answer(stream_of(&[
        text_chunk("Sure: <EMA"),
        text_chunk("IL_1> and <EMAIL_2>."),
    ]));
    let chat = json!({ "model": "gpt", "stream": true,
        "messages": [{ "role": "user", "content": "Mail bob@x.io, or <alice@y.org>." }] });
    let got = send(&gw, chat).await;
    show("a chat", &fake, got);

    // The model calls a tool, the placeholder split across its arguments.
    fake.answer(stream_of(&[
        call_chunk(Some("send_mail"), r#"{"to":"<EMAIL_"#),
        call_chunk(None, r#"1>","subject":"Q3"}"#),
    ]));
    let tool = json!({ "type": "function", "function": { "name": "send_mail",
        "description": "Send an e-mail.", "parameters": { "type": "object" } } });
    let chat = json!({ "model": "gpt", "stream": true, "tools": [tool],
        "messages": [{ "role": "user", "content": "Send the Q3 report to bob@x.io." }] });
    let got = send(&gw, chat).await;
    show("a tool call", &fake, got);
    Ok(())
}

/// Values hidden behind placeholders, `<EMAIL_n>`, for one request; clones
/// share them.
#[derive(Clone, Default)]
struct Vault(Arc<Mutex<Vec<(String, String)>>>);

impl Vault {
    /// The placeholder of `value`, made on first sight.
    fn hide(&self, value: &str) -> String {
        let mut all = self.0.lock();
        if let Some((placeholder, _)) = all.iter().find(|(_, v)| v == value) {
            return placeholder.clone();
        }
        let placeholder = format!("<EMAIL_{}>", all.len() + 1);
        all.push((placeholder.clone(), value.to_owned()));
        placeholder
    }

    /// `text` with each placeholder replaced by its value; in JSON text,
    /// such as a tool call's arguments, the value is escaped.
    fn restore(&self, text: &str, json: bool) -> String {
        let mut text = text.to_owned();
        for (placeholder, value) in self.0.lock().iter() {
            let value = match json {
                true => Value::from(value.as_str())
                    .to_string()
                    .trim_matches('"')
                    .to_owned(),
                false => value.clone(),
            };
            text = text.replace(placeholder.as_str(), &value);
        }
        text
    }

    /// How many trailing bytes of `text` may start a placeholder.
    fn pending(&self, text: &str) -> usize {
        let all = self.0.lock();
        let starts = |placeholder: &str| {
            (1..placeholder.len())
                .rev()
                .find(|&n| text.ends_with(&placeholder[..n]))
        };
        all.iter().filter_map(|(p, _)| starts(p)).max().unwrap_or(0)
    }
}

/// `text` with every e-mail address hidden in `vault`. A plain scan; put a
/// real detector here (a regex set for phone numbers and more, an NER model).
fn hide_emails(text: &str, vault: &Vault) -> String {
    let local = |c: char| c.is_ascii_alphanumeric() || "._%+-".contains(c);
    let domain = |c: char| c.is_ascii_alphanumeric() || ".-".contains(c);
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('@') {
        let user: usize = rest[..at]
            .chars()
            .rev()
            .take_while(|&c| local(c))
            .map(char::len_utf8)
            .sum();
        let host: usize = rest[at + 1..].chars().take_while(|&c| domain(c)).count();
        let host = rest[at + 1..at + 1 + host].trim_end_matches('.');
        let (start, end) = (at - user, at + 1 + host.len());
        if user > 0 && host.contains('.') {
            out.push_str(&rest[..start]);
            out.push_str(&vault.hide(&rest[start..end]));
        } else {
            out.push_str(&rest[..end]);
        }
        rest = &rest[end..];
    }
    out + rest
}

fn refusal(message: &str) -> Fault {
    let message = format!("the redactor cannot read {message}, so it refuses it");
    Fault::new(message)
        .with(ErrorCode("unreadable_content".into()))
        .with(Status(StatusCode::UNPROCESSABLE_ENTITY))
}

/// Every text of `chat` redacted: messages and the system prompt, earlier
/// tool calls and results, the tools offered, the stop sequences, the end
/// user's id, and the fields the kind has no place for. `Err` for a part
/// with no text form, such as an image.
fn redact(chat: &mut Input, vault: &Vault) -> Result<(), Fault> {
    fn part(content: &mut Content, vault: &Vault) -> Result<(), Fault> {
        match content {
            Content::Text { text, .. } | Content::Reasoning { text, .. } => {
                *text = hide_emails(text, vault)
            }
            // JSON text: an address has no quote in it, nor has a placeholder.
            Content::ToolCall { arguments, .. } => *arguments = hide_emails(arguments, vault),
            Content::ToolResult { content, .. } => {
                content.iter_mut().try_for_each(|c| part(c, vault))?
            }
            _ => return Err(refusal("a part other than text, such as an image")),
        }
        Ok(())
    }
    let json = |fields: &mut serde_json::Map<String, Value>| {
        fields.values_mut().for_each(|v| redact_json(v, vault))
    };
    for message in &mut chat.messages {
        message
            .content
            .iter_mut()
            .try_for_each(|c| part(c, vault))?;
        message.native.iter_mut().for_each(|n| json(&mut n.fields));
    }
    for tool in &mut chat.tools {
        tool.description
            .iter_mut()
            .for_each(|t| *t = hide_emails(t, vault));
        redact_json(&mut tool.parameters, vault);
    }
    for text in chat.options.stop.iter_mut().chain(&mut chat.user) {
        *text = hide_emails(text, vault);
    }
    chat.native.iter_mut().for_each(|n| json(&mut n.fields));
    Ok(())
}

/// Every string in `value` redacted.
fn redact_json(value: &mut Value, vault: &Vault) {
    match value {
        Value::String(s) => *s = hide_emails(s, vault),
        Value::Array(items) => items.iter_mut().for_each(|v| redact_json(v, vault)),
        Value::Object(fields) => fields.values_mut().for_each(|v| redact_json(v, vault)),
        _ => {}
    }
}

/// The client edge: one vault per request, restored in every response.
struct Pii;

/// The provider edge: every call's input, redacted into the vault.
struct PiiInput;

#[async_trait]
impl Plugin for Pii {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>, Fault> {
        if !cx.is_kind::<Input>() {
            return Err(refusal("this kind of traffic"));
        }
        let vault = Vault::default();
        cx.filter(Restore::new(vault.clone()))?;
        cx.extensions_mut().insert(vault);
        Ok(None)
    }
}

#[async_trait]
impl Plugin for PiiInput {
    async fn on_provider(&self, px: &mut ProviderContext) -> Result<(), Fault> {
        if !px.is_kind::<Input>() {
            return Err(refusal("this kind of traffic"));
        }
        let vault = match px.extensions().get::<Vault>() {
            Some(vault) => vault.clone(),
            // A call a plugin before `Pii` makes: restore its response here.
            None => {
                let vault = Vault::default();
                px.filter(Restore::new(vault.clone()))?;
                vault
            }
        };
        px.filter(move |chat: &mut Input| redact(chat, &vault))
    }
}

/// A response filter that restores placeholders, holding back, per block,
/// the tail of a text or a tool call's arguments that may start one until
/// the next delta or the block's end says.
struct Restore {
    vault: Vault,
    held: BTreeMap<u32, Delta>,
}

impl Restore {
    fn new(vault: Vault) -> Self {
        let held = BTreeMap::new();
        Self { vault, held }
    }

    /// `delta`'s text restored, as a delta of `block`, unless empty.
    fn restored(&self, block: u32, mut delta: Delta, out: &mut Vec<Output>) {
        let json = matches!(delta, Delta::Arguments(_));
        let text = self.vault.restore(delta.as_str(), json);
        if !text.is_empty() {
            *delta.as_mut_string() = text;
            out.push(Output::Delta { block, delta });
        }
    }

    fn release(&mut self, block: u32, out: &mut Vec<Output>) {
        if let Some(delta) = self.held.remove(&block) {
            self.restored(block, delta, out);
        }
    }

    fn release_all(&mut self, out: &mut Vec<Output>) {
        let blocks: Vec<u32> = self.held.keys().copied().collect();
        blocks
            .into_iter()
            .for_each(|block| self.release(block, out));
    }
}

impl Filter<Output> for Restore {
    fn edit(&mut self, event: Output, out: &mut Vec<Output>) -> Result<(), Fault> {
        match event {
            Output::Delta { block, mut delta } => {
                if let Some(held) = self.held.remove(&block) {
                    delta.as_mut_string().insert_str(0, held.as_str());
                }
                let at = delta.as_str().len() - self.vault.pending(delta.as_str());
                let tail = delta.as_mut_string().split_off(at);
                if !tail.is_empty() {
                    let mut kept = delta.clone();
                    *kept.as_mut_string() = tail;
                    self.held.insert(block, kept);
                }
                self.restored(block, delta, out);
            }
            Output::BlockStart {
                block,
                kind: Block::ToolCall { id, name },
            } => {
                let name = self.vault.restore(&name, false);
                out.push(Output::block_start(block, Block::ToolCall { id, name }));
            }
            Output::BlockEnd { block } => {
                self.release(block, out);
                out.push(event);
            }
            Output::Finish(_) => {
                self.release_all(out);
                out.push(event);
            }
            Output::Native(_) => return Err(refusal("an event of one format")),
            other => out.push(other),
        }
        Ok(())
    }

    fn end(&mut self, _end: &Result<(), Fault>, out: &mut Vec<Output>) -> Result<(), Fault> {
        self.release_all(out);
        Ok(())
    }
}

/// `Pii` first, `PiiInput` last.
fn gateway(base_url: &str) -> Result<Gateway, Fault> {
    let mut key = HeaderMap::new();
    key.insert("authorization", HeaderValue::from_static("Bearer sk-test"));
    let transport = HttpTransport::new(base_url)?.with_headers(key)?.connect();
    Gateway::new()
        .provider(Provider::new("openai", OpenAiChat, transport))?
        .client::<Input, _, _>(OpenAiChat)?
        .plugin(Pii)
        .plugin(PiiInput)
        .target("openai")
}

fn text_chunk(text: &str) -> Value {
    json!({ "content": text })
}

/// A chunk of a streamed call of tool `name` (given in the first chunk).
fn call_chunk(name: Option<&str>, arguments: &str) -> Value {
    let mut call = json!({ "index": 0, "function": { "arguments": arguments } });
    if let Some(name) = name {
        call["id"] = "call_1".into();
        call["type"] = "function".into();
        call["function"]["name"] = name.into();
    }
    json!({ "tool_calls": [call] })
}

/// A provider's answer streamed as OpenAI chat chunks, one per delta.
fn stream_of(deltas: &[Value]) -> Answer {
    let chunk = |delta: &Value| {
        let chunk = json!({ "id": "c1", "model": "gpt",
            "choices": [{ "index": 0, "delta": delta }] });
        Value::String(format!("data: {chunk}\n\n"))
    };
    let done = Value::String("data: [DONE]\n\n".into());
    Answer::events(deltas.iter().map(chunk).chain([done]).collect())
}

/// Send `chat`; the status and the text and tool arguments the client got.
async fn send(gw: &Gateway, chat: Value) -> (StatusCode, String) {
    let mut req = Request::new(Body::full(chat.to_string()));
    *req.method_mut() = Method::POST;
    *req.uri_mut() = "/v1/chat/completions".parse().expect("a path");
    let (resp, _) = respan_gateway_http::handle(gw, req, 1 << 20).await;
    let status = resp.status();
    let body = match resp.into_body().collect().await {
        Ok(body) => String::from_utf8_lossy(&body.to_bytes()).into_owned(),
        Err(fault) => format!("[stream cut: {fault}]"),
    };
    let events = body.lines().filter_map(|l| l.strip_prefix("data: "));
    let json = events.filter_map(|d| serde_json::from_str::<Value>(d).ok());
    let said: String = json
        .map(|v| {
            let delta = &v["choices"][0]["delta"];
            let args = &delta["tool_calls"][0]["function"]["arguments"];
            let part = delta["content"].as_str().or(args.as_str());
            part.unwrap_or_default().to_owned()
        })
        .collect();
    (status, if said.is_empty() { body } else { said })
}

/// What the provider was sent of the last chat, and what the client got.
fn show(what: &str, fake: &FakeOpenAi, (status, got): (StatusCode, String)) {
    let sent = fake.requests().last().map(|r| r.raw.to_vec());
    let sent: Value = serde_json::from_slice(&sent.unwrap_or_default()).unwrap_or_default();
    let prompt = &sent["messages"][0]["content"];
    println!("{what}\n  provider was sent: {prompt}\n  client got ({status}): {got}");
}
