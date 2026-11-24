#![warn(missing_docs)]
//! The responses plugin ([`Responses`]): a Responses client's
//! `previous_response_id`, kept by the gateway for a provider that keeps
//! nothing, in a [`Store`] ([`Memory`] unless told otherwise).
//!
//! A response is given an id the gateway mints (`resp_…`, random, so no
//! provider's ids collide or are guessed) and kept as its turn (the
//! messages its request added, and the reply, with the calls the gateway ran
//! in it and their results) beside the response it
//! followed; a follow-up naming it is sent the conversation, read back by
//! following them. A request that says `store: false` is not kept, and
//! OpenAI's own Responses keep theirs.
//!
//! What is kept is reached by its id alone, which none can guess.
//!
//! It also serves OpenAI's Conversations API, of the Responses format
//! ([`conversations`](respan_gateway_conversation_http::responses::conversations)),
//! from conversations it keeps in [`Conversations`], answering each of its
//! requests itself. A Responses request that names a `conversation` is sent
//! its items before its input, whatever the provider, and once its response
//! ended well, its input items and the response's output items are added to
//! it. It cannot name a `previous_response_id` too. A conversation made under
//! a [`Scope`] is reached only under it.

mod store;

use parking_lot::{Mutex, MutexGuard};
use std::sync::Arc;
use std::time::Duration;

use respan_gateway_conversation::{Assembled, Content, Input, Message, Output, Role};
use respan_gateway_conversation_http::Schema;
use respan_gateway_conversation_http::responses::OpenAiResponses;
use respan_gateway_conversation_http::responses::conversations::{self, Operation};
use respan_gateway_core::futures::stream;
use respan_gateway_core::{
    ClientContext, Fault, Filter, Native, Plugin, ProviderContext, Response, Result, async_trait,
};
use respan_gateway_http::raw::Original;
use serde_json::{Value, json};

pub use store::{Conversation, Conversations, Memory, Store, Turn};

/// Who a request is of, such as its project, as the host that knows says,
/// in its extensions before this plugin: a conversation made under one is
/// reached only under the same, and one made under none only under none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope(pub String);

/// How many responses a conversation follows back at most.
const MAX_DEPTH: usize = 1_000;

const RESPONSES: &str = OpenAiResponses::NAME;

/// The plugin: register it before the plugins that send a request on, such
/// as routing's dispatch.
pub struct Responses {
    /// How long a turn is kept, as the refusal of one not kept says.
    keep: Duration,
    store: Arc<dyn Store>,
    conversations: Arc<dyn Conversations>,
}

impl std::fmt::Debug for Responses {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Responses")
            .field("keep", &self.keep)
            .finish_non_exhaustive()
    }
}

impl Responses {
    /// Turns and conversations kept in this gateway's memory ([`Memory`])
    /// for `keep`.
    pub fn new(keep: Duration) -> Self {
        let memory = Arc::new(Memory::new(keep));
        Self {
            keep,
            store: memory.clone(),
            conversations: memory,
        }
    }

    /// Turns kept in `store`, which keeps each for `keep`; conversations in
    /// this gateway's memory, for `keep` too.
    pub fn with_store(keep: Duration, store: impl Store) -> Self {
        Self {
            keep,
            store: Arc::new(store),
            conversations: Arc::new(Memory::new(keep)),
        }
    }

    /// Conversations kept in `store`.
    #[must_use]
    pub fn with_conversations(mut self, store: impl Conversations) -> Self {
        self.conversations = Arc::new(store);
        self
    }

    /// The answer to `operation`, of a request in `scope`.
    async fn operate(&self, operation: Operation, scope: Option<String>) -> Result<Value> {
        let store = &*self.conversations;
        if let Operation::Create { items, metadata } = operation {
            let id = new_id("conv");
            let body = conversations::conversation(&id, now(), &metadata);
            let conversation = Conversation {
                created_at: now(),
                metadata,
                scope,
                items,
            };
            store.create(id, conversation).await?;
            return Ok(body);
        }
        let id = operation.conversation().unwrap_or_default().to_owned();
        let kept = joined(store, &id, scope.as_deref()).await?;
        let gone = || conversations::no_conversation(&id);
        Ok(match operation {
            Operation::Retrieve { .. } => {
                conversations::conversation(&id, kept.created_at, &kept.metadata)
            }
            Operation::Update { metadata, .. } => {
                let mut merged = kept.metadata.clone();
                merged.extend(metadata.clone());
                let (most, _, _) = conversations::METADATA;
                if merged.len() > most {
                    let had = kept.metadata.len();
                    return Err(conversations::too_much_metadata(merged.len(), had));
                }
                if !store.update(&id, metadata).await? {
                    return Err(gone());
                }
                conversations::conversation(&id, kept.created_at, &merged)
            }
            Operation::Delete { .. } => {
                if !store.delete(&id).await? {
                    return Err(gone());
                }
                conversations::deleted(&id)
            }
            Operation::List { page, .. } => conversations::page_of(&kept.items, &page)?,
            Operation::Add {
                items, encrypted, ..
            } => {
                if !store.append(&id, items.clone()).await? {
                    return Err(gone());
                }
                conversations::item_list(conversations::shown(items, encrypted), false)
            }
            Operation::Item {
                item, encrypted, ..
            } => {
                let found = kept.items.into_iter().find(|i| i["id"] == item.as_str());
                let found = found.ok_or_else(|| conversations::no_item(&item, true))?;
                conversations::shown(vec![found], encrypted).remove(0)
            }
            Operation::DeleteItem { item, .. } => {
                if !store.remove(&id, &item).await? {
                    return Err(conversations::no_item(&item, false));
                }
                conversations::conversation(&id, kept.created_at, &kept.metadata)
            }
            _ => return Err(Fault::not_found("no such conversations operation")),
        })
    }
}

/// Conversation `id`, if kept in `scope`.
async fn joined(store: &dyn Conversations, id: &str, scope: Option<&str>) -> Result<Conversation> {
    let kept = store.get(id).await?;
    let kept = kept.filter(|c| c.scope.as_deref() == scope);
    kept.ok_or_else(|| conversations::no_conversation(id))
}

/// Now, in seconds since the Unix epoch.
fn now() -> u64 {
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    since.map_or(0, |d| d.as_secs())
}

/// The conversation up to and including response `id`, oldest first, if
/// none of it expired.
async fn conversation(store: &dyn Store, id: &str) -> Option<Vec<Message>> {
    let mut turns = Vec::new();
    let mut next = Some(id.to_owned());
    while let Some(id) = next.take() {
        let turn = store.get(&id).await?;
        if turns.len() == MAX_DEPTH {
            return None;
        }
        next = turn.parent.clone();
        turns.push(turn);
    }
    Some(turns.into_iter().rev().flat_map(|t| t.messages).collect())
}

/// What a client's request adds to its conversation, once a call of it is
/// translated; shared by the request's client edge and its calls.
#[derive(Clone, Default)]
struct Pending(Arc<Mutex<Shared>>);

#[derive(Default)]
struct Shared {
    /// What the first translated call added.
    request: Option<Request>,
    /// Whether the request named `instructions`, a leading system message
    /// apart from its input.
    instructed: bool,
    /// Whether a call went to OpenAI's own Responses, which keeps its
    /// response itself: the response then is not the gateway's to keep, as
    /// which call answered (a hedge's race) is not known here.
    openai: bool,
    /// The conversation the request named: its id, and its items as
    /// messages, read before any call.
    conversation: Option<(String, Vec<Message>)>,
    /// What its response, ended well, adds to its conversation.
    appended: Option<(String, Vec<Value>)>,
}

struct Request {
    parent: Option<String>,
    added: Vec<Message>,
    store: bool,
}

impl Pending {
    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.0.lock()
    }
}

#[async_trait]
impl Plugin for Responses {
    async fn on_client(
        &self,
        cx: &mut ClientContext,
    ) -> Result<Option<respan_gateway_core::Response>> {
        if cx.format() != RESPONSES || !cx.is_kind::<Input>() {
            return Ok(None);
        }
        let scope = cx.extensions().get::<Scope>().map(|s| s.0.clone());
        let original = cx.extensions().get::<Original>().and_then(Original::get);
        if let Some(operation) = original.and_then(Operation::read) {
            let body = self.operate(operation?, scope).await?;
            let answer = stream::iter([Ok(conversations::answer(body))]);
            return Ok(Some(Response::stream(answer)));
        }
        let pending = Pending::default();
        pending.lock().instructed = original.is_some_and(|r| OpenAiResponses::instructed(r.body()));
        if let Ok(input) = cx.input::<Input>() {
            let body = original.map(|r| r.body().as_ref()).unwrap_or_default();
            let named = conversations::conversation_of(body)?;
            if let Some(id) = named {
                if previous_response_id(input).is_some() {
                    return Err(conversations::both_named());
                }
                let kept = joined(&*self.conversations, &id, scope.as_deref()).await?;
                let before = conversations::messages(&kept.items)?;
                pending.lock().conversation = Some((id, before));
            }
        }
        cx.extensions_mut().insert(pending.clone());
        cx.filter(Keep {
            store: self.store.clone(),
            pending,
            id: None,
            conversation: None,
            said: Assembled::default(),
        })?;
        Ok(None)
    }

    async fn on_client_end(&self, cx: &ClientContext, _end: &Result<()>) {
        let Some(pending) = cx.extensions().get::<Pending>() else {
            return;
        };
        let appended = pending.lock().appended.take();
        if let Some((id, items)) = appended {
            // WARN: what a response adds to a conversation that failed to keep it is lost.
            let _ = self.conversations.append(&id, items).await;
        }
    }

    async fn on_provider(&self, px: &mut ProviderContext) -> Result<()> {
        // A call of another kind, such as a tool loop's to its tool server,
        // carries the request's extensions too.
        if !px.is_kind::<Input>() || px.client().format() != RESPONSES {
            return Ok(());
        }
        let Some(pending) = px.extensions().get::<Pending>().cloned() else {
            return Ok(());
        };
        let (keep, target) = (self.keep, px.format().to_owned());
        // Read before the call, as a shared store answers in its own time.
        let parent = px.input::<Input>().ok().and_then(previous_response_id);
        let before = match &parent {
            Some(id) => conversation(&*self.store, id).await,
            None => None,
        };
        let joined = pending.lock().conversation.clone();
        // OpenAI's own Responses keep its responses, but not the gateway's:
        // a conversation that crossed to another provider (a fallback's) is
        // sent whole, as to one that keeps nothing. A conversation is the
        // gateway's alone, sent whole to any.
        if px.format() == RESPONSES {
            pending.lock().openai = true;
            return px.filter(move |input: &mut Input| {
                let at = instructions(&pending, input);
                // A tool loop's later call names it no more, its items sent.
                let named = OpenAiResponses::fields(&input.native).contains_key("conversation");
                if let Some((_, before)) = joined.as_ref().filter(|_| named) {
                    let added = input.messages[at..].to_vec();
                    pending.lock().request.get_or_insert(Request {
                        parent: None,
                        added,
                        store: keeps(&input.native),
                    });
                    input.native = OpenAiResponses::with_fields(&input.native, |f| {
                        f.shift_remove("conversation");
                        include_encrypted(f);
                    });
                    let before = before.iter().cloned().map(|m| writable(m, &target));
                    input.messages.splice(at..at, before);
                    return Ok(());
                }
                let Some(before) = before.clone() else {
                    return Ok(());
                };
                input.native = OpenAiResponses::with_fields(&input.native, |f| {
                    f.shift_remove("previous_response_id");
                });
                let before = before.into_iter().map(|m| writable(m, &target));
                input.messages.splice(at..at, before);
                Ok(())
            });
        }
        px.filter(move |input: &mut Input| {
            let kept = keeps(&input.native);
            let parent = previous_response_id(input);
            let fields = OpenAiResponses::fields(&input.native);
            let named = fields.contains_key("conversation");
            if fields.contains_key("previous_response_id") || named {
                input.native = OpenAiResponses::with_fields(&input.native, |f| {
                    f.shift_remove("previous_response_id");
                    f.shift_remove("conversation");
                });
            }
            let instructions = instructions(&pending, input);
            let added = input.messages[instructions..].to_vec();
            if let Some((_, before)) = joined.as_ref().filter(|_| named) {
                let before = before.iter().cloned().map(|m| writable(m, &target));
                input.messages.splice(instructions..instructions, before);
            }
            if let Some(id) = &parent {
                let before = before.clone().ok_or_else(|| {
                    Fault::bad_request(format!(
                        "previous_response_id '{id}' is no response this gateway keeps: it keeps \
                         a response of a provider of another format for {}s, in a conversation of up to {MAX_DEPTH} responses",
                        keep.as_secs()
                    ))
                })?;
                let before = before.into_iter().map(|m| writable(m, &target));
                input.messages.splice(instructions..instructions, before);
            }
            // A tool loop's later calls add its own steps: the client's
            // request is what its first call added.
            pending.lock().request.get_or_insert(Request {
                parent,
                added,
                store: kept,
            });
            Ok(())
        })
    }
}

/// Asks OpenAI for its reasoning encrypted, as the conversation it is kept
/// in sends it back to a call that may not reach it otherwise.
fn include_encrypted(fields: &mut serde_json::Map<String, Value>) {
    let include = fields.entry("include").or_insert_with(|| json!([]));
    let encrypted = json!("reasoning.encrypted_content");
    if let Some(include) = include.as_array_mut()
        && !include.contains(&encrypted)
    {
        include.push(encrypted);
    }
}

/// Whether a request's own fields let its response be kept: `store: false`
/// does not.
fn keeps(native: &Native) -> bool {
    OpenAiResponses::fields(native).get("store") != Some(&Value::Bool(false))
}

/// How many of `input`'s messages are the request's instructions, not its
/// conversation: a leading system message, if it named `instructions`.
fn instructions(pending: &Pending, input: &Input) -> usize {
    let leading = input
        .messages
        .first()
        .is_some_and(|m| m.role == Role::System);
    usize::from(pending.lock().instructed && leading)
}

/// The `previous_response_id` a Responses request names.
fn previous_response_id(input: &Input) -> Option<String> {
    let fields = OpenAiResponses::fields(&input.native);
    let parent = fields.get("previous_response_id")?.as_str()?;
    Some(parent.to_owned())
}

/// `message` without what `format` cannot write: reasoning sealed by a
/// signature, which only its own provider reads back, and the signatures
/// that seal it, unless `format` is Responses, whose writer keeps signed
/// reasoning as its own. Plain reasoning is the kind's, which any writes.
fn writable(mut message: Message, format: &str) -> Message {
    let mut is_dropped = false;
    let mut parts = std::mem::take(&mut message.content).into_iter().peekable();
    while let Some(part) = parts.next() {
        is_dropped = match part {
            // WARN: signed reasoning is dropped for another format than Responses.
            Content::Reasoning { .. } => {
                format != RESPONSES && matches!(parts.peek(), Some(Content::Signature(_)))
            }
            Content::Signature(_) => is_dropped,
            _ => false,
        };
        if !is_dropped {
            message.content.push(part);
        }
    }
    message
}

/// The response its client is given, named anew and kept once it ended well.
struct Keep {
    store: Arc<dyn Store>,
    pending: Pending,
    /// Its id, once named.
    id: Option<String>,
    /// The conversation it is added to, once it started.
    conversation: Option<String>,
    said: Assembled,
}

impl Filter<Output> for Keep {
    fn edit(&mut self, mut output: Output, out: &mut Vec<Output>) -> Result<()> {
        if let Output::Start(start) = &mut output {
            let shared = self.pending.lock();
            if !shared.openai && shared.request.as_ref().is_some_and(|r| r.store) {
                let id = new_id("resp");
                start.id.clone_from(&id);
                self.id = Some(id);
            }
            if let Some((id, _)) = &shared.conversation {
                start.native = OpenAiResponses::with_fields(&start.native, |f| {
                    f.insert("conversation".into(), json!({ "id": id }));
                });
                self.conversation = Some(id.clone());
            }
        }
        if self.id.is_some() || self.conversation.is_some() {
            // What cannot be folded is only not kept; the response goes on.
            let _ = self.said.push(&output);
        }
        out.push(output);
        Ok(())
    }

    fn end(&mut self, end: &Result<()>, _out: &mut Vec<Output>) -> Result<()> {
        let shared = std::mem::take(&mut *self.pending.lock());
        // As OpenAI's: a request that says `store: false` reads its
        // conversation, and adds nothing to it.
        if let (Ok(()), Some(id), Some(request)) =
            (end, self.conversation.take(), shared.request.as_ref())
            && request.store
        {
            // What cannot be written as items is only not kept.
            let input = conversations::input_items(&request.added).unwrap_or_default();
            let output = conversations::output_items(&self.said).unwrap_or_default();
            let items = input.into_iter().chain(output).collect();
            self.pending.lock().appended = Some((id, items));
        }
        let (Ok(()), Some(id), Some(request), false) =
            (end, self.id.take(), shared.request, shared.openai)
        else {
            return Ok(());
        };
        // The calls the gateway ran, with their results, are its turn's too.
        let mut messages = request.added;
        messages.extend(self.said.messages());
        let turn = Turn {
            parent: request.parent,
            messages,
        };
        self.store.put(id, turn);
        Ok(())
    }
}

/// An id of the gateway's, a response's (`resp`) or a conversation's
/// (`conv`): `prefix_` and a random UUID's hex.
fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_conversation::Block;
    use respan_gateway_conversation_http::openai::OpenAiChat;
    use respan_gateway_core::futures::{FutureExt, StreamExt};
    use respan_gateway_core::{Gateway, Provider};
    use respan_gateway_http::Body;
    use respan_gateway_http::bytes::Bytes;
    use respan_gateway_http::http;
    use std::collections::HashMap;

    /// Turns two gateways share, read back only after a yield, as over a
    /// network.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<HashMap<String, Turn>>>);

    #[async_trait]
    impl Store for Shared {
        fn put(&self, key: String, turn: Turn) {
            self.0.lock().insert(key, turn);
        }

        async fn get(&self, key: &str) -> Option<Turn> {
            tokio::task::yield_now().await;
            self.0.lock().get(key).cloned()
        }
    }

    /// A provider's connection, answering `reply` and keeping each body it
    /// is sent.
    fn answering(
        reply: Value,
        sent: Arc<Mutex<Vec<Value>>>,
    ) -> impl Fn(respan_gateway_core::Extensions) -> respan_gateway_http::ProviderIo
    + Clone
    + Send
    + Sync
    + 'static {
        respan_gateway_http::connect(
            move |request: http::Request<Body>| {
                let (sent, reply) = (Arc::clone(&sent), reply.clone());
                async move {
                    let chunks: Vec<_> = request.into_body().collect().await;
                    let body: Vec<u8> = chunks
                        .into_iter()
                        .flat_map(|c| c.map(|c| c.to_vec()).unwrap_or_default())
                        .collect();
                    sent.lock()
                        .push(serde_json::from_slice(&body).unwrap_or_default());
                    let mut response = http::Response::new(Body::full(reply.to_string()));
                    let json = http::HeaderValue::from_static("application/json");
                    response
                        .headers_mut()
                        .insert(http::header::CONTENT_TYPE, json);
                    Ok((response, None))
                }
                .boxed()
            },
            1 << 20,
        )
    }

    /// A gateway over `store` whose one provider speaks Chat Completions,
    /// answering `answer` and keeping each body it is sent.
    fn translating(store: Shared, answer: &'static str, sent: Arc<Mutex<Vec<Value>>>) -> Gateway {
        let reply = serde_json::json!({
            "id": "chatcmpl-1", "object": "chat.completion", "created": 1, "model": "m",
            "choices": [{"index": 0, "finish_reason": "stop",
                "message": {"role": "assistant", "content": answer}}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
        });
        let connect = answering(reply, sent);
        Gateway::new()
            .client::<Input, Output, _>(OpenAiResponses)
            .and_then(|g| g.provider(Provider::new("chat", OpenAiChat, connect)))
            .and_then(|g| g.target("chat"))
            .map(|g| g.plugin(Responses::with_store(Duration::from_secs(60), store)))
            .expect("a gateway")
    }

    /// OpenAI's own Responses, sent no conversation id of the gateway's but
    /// its items, and asked for its reasoning encrypted.
    #[tokio::test]
    async fn openai_is_sent_a_conversation_whole() {
        let reply = serde_json::json!({
            "id": "resp_openai", "object": "response", "created_at": 1, "model": "m", "status": "completed",
            "output": [{"id": "msg_openai", "type": "message", "role": "assistant", "status": "completed",
                "content": [{"type": "output_text", "text": "Paris", "annotations": []}]}],
            "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2},
        });
        let sent = Arc::new(Mutex::new(Vec::new()));
        let connect = answering(reply, Arc::clone(&sent));
        let gateway = Gateway::new()
            .client::<Input, Output, _>(OpenAiResponses)
            .and_then(|g| g.provider(Provider::new("openai", OpenAiResponses, connect)))
            .and_then(|g| g.target("openai"))
            .map(|g| g.plugin(Responses::new(Duration::from_secs(60))))
            .expect("a gateway");
        let made = serde_json::json!({"items": [{"role": "user", "content": "I am in Europe."}]});
        let (_, made) = send(
            &gateway,
            http::Method::POST,
            "/v1/conversations",
            Some(made),
        )
        .await;
        let id = made["id"].as_str().expect("an id");
        let asked =
            serde_json::json!({"model": "m", "input": "capital of France?", "conversation": id});
        let answer = respond(&gateway, asked).await;
        assert_eq!(
            (answer["id"].clone(), answer["conversation"]["id"].clone()),
            (serde_json::json!("resp_openai"), serde_json::json!(id))
        );
        let call = sent.lock()[0].clone();
        assert_eq!(call.get("conversation"), None, "{call}");
        assert_eq!(
            call["include"],
            serde_json::json!(["reasoning.encrypted_content"])
        );
        let texts: Vec<&str> = (call["input"].as_array().expect("input").iter())
            .filter_map(|i| i["content"].as_str())
            .collect();
        assert_eq!(texts, ["I am in Europe.", "capital of France?"]);
        let path = format!("/v1/conversations/{id}/items/msg_openai");
        let (status, item) = send(&gateway, http::Method::GET, &path, None).await;
        assert_eq!(status, http::StatusCode::OK, "{item}");
    }

    async fn respond(gateway: &Gateway, body: Value) -> Value {
        send(gateway, http::Method::POST, "/v1/responses", Some(body))
            .await
            .1
    }

    /// The status and body of `method` at `path`, its body `body`.
    async fn send(
        gateway: &Gateway,
        method: http::Method,
        path: &str,
        body: Option<Value>,
    ) -> (http::StatusCode, Value) {
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let mut request = http::Request::new(Body::full(body));
        *request.method_mut() = method;
        *request.uri_mut() = path.parse().expect("a path");
        let (response, _) = respan_gateway_http::handle(gateway, request, 1 << 20).await;
        let status = response.status();
        let chunks: Vec<_> = response.into_body().collect().await;
        let bytes: Vec<u8> = chunks
            .into_iter()
            .flat_map(|c| c.map(|c| c.to_vec()).unwrap_or_default())
            .collect();
        gateway.idle().await;
        (status, serde_json::from_slice(&bytes).unwrap_or_default())
    }

    /// What `sent`'s call `n` said, its messages' text in order.
    fn said(sent: &Mutex<Vec<Value>>, n: usize) -> Vec<String> {
        let sent = sent.lock();
        (sent[n]["messages"].as_array().expect("messages").iter())
            .filter_map(|m| m["content"].as_str().map(Into::into))
            .collect()
    }

    /// A conversation's items, oldest first, as its list says them: each
    /// item's role and text.
    async fn listed(gateway: &Gateway, id: &str) -> Vec<(String, String)> {
        let path = format!("/v1/conversations/{id}/items?order=asc");
        let (_, list) = send(gateway, http::Method::GET, &path, None).await;
        (list["data"].as_array().expect("data").iter())
            .map(|i| {
                let text = i["content"][0]["text"].as_str().unwrap_or_default();
                (i["role"].as_str().unwrap_or_default().into(), text.into())
            })
            .collect()
    }

    /// A conversation's items are sent before a request's input to a
    /// provider that keeps nothing, and its input and output added after.
    #[tokio::test]
    async fn a_conversation_is_sent_and_grows_with_each_response() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let gateway = translating(Shared::default(), "Paris", Arc::clone(&sent));
        let made = serde_json::json!({"items": [{"role": "user", "content": "I am in Europe."}]});
        let (status, conversation) = send(
            &gateway,
            http::Method::POST,
            "/v1/conversations",
            Some(made),
        )
        .await;
        assert_eq!(status, http::StatusCode::OK, "{conversation}");
        let id = conversation["id"].as_str().expect("an id").to_owned();
        assert!(id.starts_with("conv_"), "{conversation}");
        let asked =
            serde_json::json!({"model": "m", "input": "capital of France?", "conversation": id});
        let answer = respond(&gateway, asked).await;
        assert_eq!(answer["conversation"]["id"], id.as_str(), "{answer}");
        assert_eq!(said(&sent, 0), ["I am in Europe.", "capital of France?"]);
        let asked =
            serde_json::json!({"model": "m", "input": "and Spain?", "conversation": {"id": id}});
        respond(&gateway, asked).await;
        assert_eq!(
            said(&sent, 1),
            [
                "I am in Europe.",
                "capital of France?",
                "Paris",
                "and Spain?"
            ]
        );
        let roles = ["user", "user", "assistant", "user", "assistant"];
        let texts = [
            "I am in Europe.",
            "capital of France?",
            "Paris",
            "and Spain?",
            "Paris",
        ];
        let expected: Vec<(String, String)> = (roles.iter().zip(texts))
            .map(|(r, t)| ((*r).into(), t.into()))
            .collect();
        assert_eq!(listed(&gateway, &id).await, expected);
    }

    #[tokio::test]
    async fn a_conversation_is_kept_retrieved_and_deleted() {
        let gateway = translating(Shared::default(), "", Arc::default());
        let made = serde_json::json!({"metadata": {"topic": "geography"}});
        let (_, made) = send(
            &gateway,
            http::Method::POST,
            "/v1/conversations",
            Some(made),
        )
        .await;
        let path = format!("/v1/conversations/{}", made["id"].as_str().expect("an id"));
        let (_, got) = send(&gateway, http::Method::GET, &path, None).await;
        assert_eq!(got, made);
        let metadata = serde_json::json!({"metadata": {"topic": "history"}});
        let (_, updated) = send(&gateway, http::Method::POST, &path, Some(metadata)).await;
        assert_eq!(updated["metadata"]["topic"], "history");
        let items = format!("{path}/items");
        let added = serde_json::json!({"items": [{"role": "user", "content": "a"}, {"role": "user", "content": "b"}]});
        let (_, added) = send(&gateway, http::Method::POST, &items, Some(added)).await;
        let first = added["first_id"].as_str().expect("an id").to_owned();
        let item = format!("{items}/{first}");
        let (_, got) = send(&gateway, http::Method::GET, &item, None).await;
        assert_eq!(got["content"][0]["text"], "a");
        send(&gateway, http::Method::DELETE, &item, None).await;
        let (status, _) = send(&gateway, http::Method::GET, &item, None).await;
        assert_eq!(status, http::StatusCode::NOT_FOUND);
        let (_, deleted) = send(&gateway, http::Method::DELETE, &path, None).await;
        assert_eq!(deleted["deleted"], true);
        let (status, _) = send(&gateway, http::Method::GET, &path, None).await;
        assert_eq!(status, http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_conversation_cannot_follow_a_response_too() {
        let gateway = translating(Shared::default(), "", Arc::default());
        let (_, made) = send(&gateway, http::Method::POST, "/v1/conversations", None).await;
        let both = serde_json::json!({"model": "m", "input": "hi", "conversation": made["id"], "previous_response_id": "resp_1"});
        let (status, _) = send(&gateway, http::Method::POST, "/v1/responses", Some(both)).await;
        assert_eq!(status, http::StatusCode::BAD_REQUEST);
        for unnamed in [
            serde_json::json!({"foo": 1}),
            serde_json::json!({}),
            serde_json::json!([]),
        ] {
            let unnamed = serde_json::json!({"model": "m", "input": "hi", "conversation": unnamed});
            let (status, _) =
                send(&gateway, http::Method::POST, "/v1/responses", Some(unnamed)).await;
            assert_eq!(status, http::StatusCode::BAD_REQUEST);
        }
        let unknown = serde_json::json!({"model": "m", "input": "hi", "conversation": "conv_none"});
        let (status, _) = send(&gateway, http::Method::POST, "/v1/responses", Some(unknown)).await;
        assert_eq!(status, http::StatusCode::NOT_FOUND);
    }

    /// A request that says `store: false` reads its conversation, as OpenAI's
    /// does, and adds nothing to it.
    #[tokio::test]
    async fn a_request_that_says_store_false_adds_nothing() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let gateway = translating(Shared::default(), "Paris", Arc::clone(&sent));
        let made = serde_json::json!({"items": [{"role": "user", "content": "I am in Europe."}]});
        let (_, made) = send(
            &gateway,
            http::Method::POST,
            "/v1/conversations",
            Some(made),
        )
        .await;
        let id = made["id"].as_str().expect("an id");
        let asked = serde_json::json!({"model": "m", "input": "capital?", "conversation": id, "store": false});
        let answer = respond(&gateway, asked).await;
        assert_eq!(answer["conversation"]["id"], id, "{answer}");
        assert_eq!(said(&sent, 0), ["I am in Europe.", "capital?"]);
        assert_eq!(listed(&gateway, id).await.len(), 1);
    }

    /// Metadata is merged, as OpenAI merges it, up to its most pairs.
    #[tokio::test]
    async fn metadata_is_merged() {
        let gateway = translating(Shared::default(), "", Arc::default());
        let made = serde_json::json!({"metadata": {"topic": "a", "n": "1"}});
        let (_, made) = send(
            &gateway,
            http::Method::POST,
            "/v1/conversations",
            Some(made),
        )
        .await;
        let path = format!("/v1/conversations/{}", made["id"].as_str().expect("an id"));
        let update = serde_json::json!({"metadata": {"topic": "b"}});
        let (_, updated) = send(&gateway, http::Method::POST, &path, Some(update)).await;
        assert_eq!(
            updated["metadata"],
            serde_json::json!({"topic": "b", "n": "1"})
        );
        let (_, got) = send(&gateway, http::Method::GET, &path, None).await;
        assert_eq!(got["metadata"], updated["metadata"]);
        let many: serde_json::Map<String, Value> =
            (0..15).map(|i| (format!("k{i}"), "v".into())).collect();
        let (status, refused) = send(
            &gateway,
            http::Method::POST,
            &path,
            Some(serde_json::json!({"metadata": many})),
        )
        .await;
        assert_eq!(status, http::StatusCode::BAD_REQUEST);
        assert_eq!(refused["error"]["code"], "metadata_max_properties_exceeded");
    }

    /// An item's id is the gateway's, whatever the client said, so deleting
    /// one deletes it alone.
    #[tokio::test]
    async fn an_item_is_given_its_own_id() {
        let gateway = translating(Shared::default(), "", Arc::default());
        let (_, made) = send(&gateway, http::Method::POST, "/v1/conversations", None).await;
        let items = format!(
            "/v1/conversations/{}/items",
            made["id"].as_str().expect("an id")
        );
        let same = serde_json::json!({"role": "user", "content": "x", "id": "msg_mine"});
        let added = serde_json::json!({"items": [same.clone(), same]});
        let (_, added) = send(&gateway, http::Method::POST, &items, Some(added)).await;
        let (first, last) = (
            added["first_id"].as_str().unwrap_or_default(),
            added["last_id"].as_str().unwrap_or_default(),
        );
        assert!(first != "msg_mine" && first != last, "{added}");
        send(
            &gateway,
            http::Method::DELETE,
            &format!("{items}/{first}"),
            None,
        )
        .await;
        let (_, list) = send(&gateway, http::Method::GET, &items, None).await;
        assert_eq!(list["data"].as_array().map(Vec::len), Some(1), "{list}");
    }

    /// Reasoning's encrypted content is kept, and shown only when included.
    #[tokio::test]
    async fn encrypted_reasoning_is_listed_only_when_included() {
        let gateway = translating(Shared::default(), "", Arc::default());
        let reasoning =
            serde_json::json!({"type": "reasoning", "summary": [], "encrypted_content": "sealed"});
        let made = serde_json::json!({"items": [reasoning]});
        let (_, made) = send(
            &gateway,
            http::Method::POST,
            "/v1/conversations",
            Some(made),
        )
        .await;
        let items = format!(
            "/v1/conversations/{}/items",
            made["id"].as_str().expect("an id")
        );
        let (_, plain) = send(&gateway, http::Method::GET, &items, None).await;
        assert_eq!(plain["data"][0].get("encrypted_content"), None, "{plain}");
        let included = format!("{items}?include[]=reasoning.encrypted_content");
        let (_, included) = send(&gateway, http::Method::GET, &included, None).await;
        assert_eq!(included["data"][0]["encrypted_content"], "sealed");
    }

    /// Answers as a tool loop does: the call it makes, and before what the
    /// model then said, a call the gateway ran and its result.
    struct Ran;

    #[async_trait]
    impl Plugin for Ran {
        async fn on_client(
            &self,
            cx: &mut ClientContext,
        ) -> Result<Option<respan_gateway_core::Response>> {
            let Some(target) = cx.target().map(str::to_owned) else {
                return Ok(None);
            };
            let response = cx.call(target).await?;
            let mut events: Vec<Result<Output>> = response.outputs::<Output>().collect().await;
            let call = Block::GatewayCall {
                id: "call_1".into(),
                name: "gh__search".into(),
            };
            let result = Block::ToolResult {
                id: "call_1".into(),
                is_error: false,
            };
            let steps = [
                Output::block_start(100, call),
                Output::arguments(100, "{\"q\":\"rust\"}"),
                Output::block_end(100),
                Output::block_start(101, result),
                Output::result(101, "found rust"),
                Output::block_end(101),
            ];
            // After the reply's start.
            events.splice(1..1, steps.into_iter().map(Ok));
            Ok(Some(respan_gateway_core::Response::stream(stream::iter(
                events,
            ))))
        }
    }

    /// A reply's calls the gateway ran are kept with it, as the conversation
    /// its client would send back: a follow-up's model reads them.
    #[tokio::test]
    async fn a_replys_calls_the_gateway_ran_are_kept_as_its_tool_turns() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let gateway = translating(Shared::default(), "Paris", Arc::clone(&sent)).plugin(Ran);
        let answer = respond(
            &gateway,
            serde_json::json!({"model": "m", "input": "find rust"}),
        )
        .await;
        let kinds: Vec<&Value> = (answer["output"].as_array().into_iter().flatten())
            .map(|item| &item["type"])
            .collect();
        assert_eq!(kinds, ["mcp_call", "message"], "{answer}");
        let id = answer["id"].as_str().expect("an id");
        let asked =
            serde_json::json!({"model": "m", "input": "thanks", "previous_response_id": id});
        respond(&gateway, asked).await;
        let messages = sent.lock()[1]["messages"].clone();
        let roles: Vec<&Value> = (messages.as_array().into_iter().flatten())
            .map(|m| &m["role"])
            .collect();
        assert_eq!(
            roles,
            ["user", "assistant", "tool", "assistant", "user"],
            "{messages}"
        );
        assert_eq!(messages[1]["tool_calls"][0]["id"], "call_1");
        assert_eq!(messages[2]["content"], "found rust");
        assert_eq!(messages[3]["content"], "Paris");
    }

    /// Gives each request the scope its `x-scope` header names.
    struct Scoped;

    #[async_trait]
    impl Plugin for Scoped {
        async fn on_client(
            &self,
            cx: &mut ClientContext,
        ) -> Result<Option<respan_gateway_core::Response>> {
            let original = cx.extensions().get::<Original>().and_then(Original::get);
            let scope =
                original.and_then(|r| r.headers().get("x-scope")?.to_str().ok().map(Into::into));
            if let Some(scope) = scope {
                cx.extensions_mut().insert(Scope(scope));
            }
            Ok(None)
        }
    }

    #[tokio::test]
    async fn a_conversation_is_reached_only_in_its_scope() {
        let gateway = Gateway::new()
            .client::<Input, Output, _>(OpenAiResponses)
            .map(|g| {
                g.plugin(Scoped)
                    .plugin(Responses::new(Duration::from_secs(60)))
            })
            .expect("a gateway");
        let ask = |scope: &'static str, method: http::Method, path: String| {
            let mut request = http::Request::new(Body::full(""));
            *request.method_mut() = method;
            *request.uri_mut() = path.parse().expect("a path");
            let value = http::HeaderValue::from_static(scope);
            request.headers_mut().insert("x-scope", value);
            let gateway = gateway.clone();
            async move {
                let (response, _) = respan_gateway_http::handle(&gateway, request, 1 << 20).await;
                let status = response.status();
                let chunks: Vec<_> = response.into_body().collect().await;
                let bytes: Vec<u8> = (chunks.into_iter())
                    .flat_map(|c| c.map(|c| c.to_vec()).unwrap_or_default())
                    .collect();
                (
                    status,
                    serde_json::from_slice::<Value>(&bytes).unwrap_or_default(),
                )
            }
        };
        let (_, made) = ask("a", http::Method::POST, "/v1/conversations".into()).await;
        let path = format!("/v1/conversations/{}", made["id"].as_str().expect("an id"));
        assert_eq!(
            ask("a", http::Method::GET, path.clone()).await.0,
            http::StatusCode::OK
        );
        assert_eq!(
            ask("b", http::Method::GET, path.clone()).await.0,
            http::StatusCode::NOT_FOUND
        );
        assert_eq!(
            ask("b", http::Method::DELETE, path).await.0,
            http::StatusCode::NOT_FOUND
        );
    }

    /// Signed reasoning goes with its signature for another format; plain
    /// reasoning and a call's signature stay.
    #[test]
    fn signed_reasoning_is_written_only_to_responses() {
        let plain = Content::Reasoning {
            text: "p".into(),
            native: Native::default(),
        };
        let mut signed = Message::new(Role::Assistant, "");
        signed.content = vec![
            Content::Reasoning {
                text: "r".into(),
                native: Native::default(),
            },
            Content::Signature("theirs".into()),
            plain.clone(),
            Content::tool_call("c1", "f", "{}"),
            Content::Signature("call".into()),
        ];
        assert_eq!(writable(signed.clone(), RESPONSES), signed);
        let kept = writable(signed, OpenAiChat::NAME).content;
        assert_eq!(kept[0], plain);
        assert_eq!(kept[2..], [Content::Signature("call".into())]);
    }

    /// A follow-up reaching another gateway than its response is sent the
    /// whole conversation, read from the store they share.
    #[tokio::test]
    async fn a_follow_up_on_another_gateway_reads_the_conversation_they_share() {
        let (store, sent) = (Shared::default(), Arc::new(Mutex::new(Vec::new())));
        let first = translating(store.clone(), "Paris", Arc::clone(&sent));
        let second = translating(store, "2M", Arc::clone(&sent));
        let asked = serde_json::json!({"model": "m", "input": "capital of France?"});
        let answer = respond(&first, asked).await;
        let id = answer["id"].as_str().expect("an id").to_owned();
        assert!(id.starts_with("resp_"), "{answer}");
        let follow_up = serde_json::json!({"model": "m", "input": "its population?", "previous_response_id": id});
        respond(&second, follow_up).await;
        assert_eq!(
            said(&sent, 1),
            ["capital of France?", "Paris", "its population?"]
        );
    }

    #[test]
    fn a_request_that_says_store_false_is_not_kept() {
        let kept = |body: &'static str| {
            let input = OpenAiResponses
                .decode_request(Bytes::from_static(body.as_bytes()))
                .expect("a request");
            keeps(&input.native)
        };
        assert!(!kept(r#"{"model": "m", "input": "hi", "store": false}"#));
        assert!(kept(r#"{"model": "m", "input": "hi", "store": true}"#));
        assert!(kept(r#"{"model": "m", "input": "hi"}"#));
    }
}
