#![warn(missing_docs)]
//! The responses plugin ([`Responses`]): a Responses client's
//! `previous_response_id`, kept by the gateway for a provider that keeps
//! nothing, in a [`Store`] ([`Memory`] unless told otherwise).
//!
//! A response is given an id the gateway mints (`resp_…`, random, so no
//! provider's ids collide or are guessed) and kept as its turn (the
//! messages its request added, and the reply) beside the response it
//! followed; a follow-up naming it is sent the conversation, read back by
//! following them. A request that says `store: false` is not kept, and
//! OpenAI's own Responses keep theirs.
//!
//! What is kept is reached by its id alone, which none can guess.

mod store;

use parking_lot::{Mutex, MutexGuard};
use std::sync::Arc;
use std::time::Duration;

use respan_gateway_conversation::{Assembled, Input, Message, Output, Role};
use respan_gateway_conversation_http::Schema;
use respan_gateway_conversation_http::responses::OpenAiResponses;
use respan_gateway_core::{
    ClientContext, Fault, Filter, Native, Plugin, ProviderContext, Result, async_trait,
};
use respan_gateway_http::raw::Original;
use serde_json::Value;

pub use store::{Memory, Store, Turn};

/// How many responses a conversation follows back at most.
const MAX_DEPTH: usize = 1_000;

const RESPONSES: &str = OpenAiResponses::NAME;

/// The plugin: register it before the plugins that send a request on, such
/// as routing's dispatch.
pub struct Responses {
    /// How long a turn is kept, as the refusal of one not kept says.
    keep: Duration,
    store: Arc<dyn Store>,
}

impl std::fmt::Debug for Responses {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Responses")
            .field("keep", &self.keep)
            .finish_non_exhaustive()
    }
}

impl Responses {
    /// Turns kept in this gateway's memory ([`Memory`]) for `keep`.
    pub fn new(keep: Duration) -> Self {
        Self::with_store(keep, Memory::new(keep))
    }

    /// Turns kept in `store`, which keeps each for `keep`.
    pub fn with_store(keep: Duration, store: impl Store) -> Self {
        Self {
            keep,
            store: Arc::new(store),
        }
    }
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
        let pending = Pending::default();
        let original = cx.extensions().get::<Original>().and_then(Original::get);
        pending.lock().instructed = original.is_some_and(|r| OpenAiResponses::instructed(r.body()));
        cx.extensions_mut().insert(pending.clone());
        cx.filter(Keep {
            store: self.store.clone(),
            pending,
            id: None,
            said: Assembled::default(),
        })?;
        Ok(None)
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
        let keep = self.keep;
        // Read before the call, as a shared store answers in its own time.
        let parent = px.input::<Input>().ok().and_then(previous_response_id);
        let before = match &parent {
            Some(id) => conversation(&*self.store, id).await,
            None => None,
        };
        // OpenAI's own Responses keep its responses, but not the gateway's:
        // a conversation that crossed to another provider (a fallback's) is
        // sent whole, as to one that keeps nothing.
        if px.format() == RESPONSES {
            pending.lock().openai = true;
            return px.filter(move |input: &mut Input| {
                let Some(before) = before.clone() else {
                    return Ok(());
                };
                input.native = OpenAiResponses::with_fields(&input.native, |f| {
                    f.shift_remove("previous_response_id");
                });
                let at = instructions(&pending, input);
                input.messages.splice(at..at, before);
                Ok(())
            });
        }
        px.filter(move |input: &mut Input| {
            let kept = keeps(&input.native);
            let parent = previous_response_id(input);
            if OpenAiResponses::fields(&input.native).contains_key("previous_response_id") {
                input.native = OpenAiResponses::with_fields(&input.native, |f| {
                    f.shift_remove("previous_response_id");
                });
            }
            let instructions = instructions(&pending, input);
            let added = input.messages[instructions..].to_vec();
            if let Some(id) = &parent {
                let before = before.clone().ok_or_else(|| {
                    Fault::bad_request(format!(
                        "previous_response_id '{id}' is no response this gateway keeps: it keeps \
                         a response of a provider of another format for {}s, in a conversation of up to {MAX_DEPTH} responses",
                        keep.as_secs()
                    ))
                })?;
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

/// The response its client is given, named anew and kept once it ended well.
struct Keep {
    store: Arc<dyn Store>,
    pending: Pending,
    /// Its id, once named.
    id: Option<String>,
    said: Assembled,
}

impl Filter<Output> for Keep {
    fn edit(&mut self, mut output: Output, out: &mut Vec<Output>) -> Result<()> {
        if let Output::Start(start) = &mut output {
            let shared = self.pending.lock();
            if !shared.openai && shared.request.as_ref().is_some_and(|r| r.store) {
                let id = new_id();
                start.id.clone_from(&id);
                self.id = Some(id);
            }
        }
        if self.id.is_some() {
            // What cannot be folded is only not kept; the response goes on.
            let _ = self.said.push(&output);
        }
        out.push(output);
        Ok(())
    }

    fn end(&mut self, end: &Result<()>, _out: &mut Vec<Output>) -> Result<()> {
        let shared = std::mem::take(&mut *self.pending.lock());
        let (Ok(()), Some(id), Some(request), false) =
            (end, self.id.take(), shared.request, shared.openai)
        else {
            return Ok(());
        };
        let mut reply = Message::new(Role::Assistant, "");
        reply.content = self.said.replay();
        let mut messages = request.added;
        messages.push(reply);
        let turn = Turn {
            parent: request.parent,
            messages,
        };
        self.store.put(id, turn);
        Ok(())
    }
}

/// A response id of the gateway's: `resp_` and a random UUID's hex.
fn new_id() -> String {
    format!("resp_{}", uuid::Uuid::new_v4().simple())
}

#[cfg(test)]
mod tests {
    use super::*;
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

    /// A gateway over `store` whose one provider speaks Chat Completions,
    /// answering `answer` and keeping each body it is sent.
    fn translating(store: Shared, answer: &'static str, sent: Arc<Mutex<Vec<Value>>>) -> Gateway {
        let connect = respan_gateway_http::connect(
            move |request: http::Request<Body>| {
                let sent = Arc::clone(&sent);
                async move {
                    let chunks: Vec<_> = request.into_body().collect().await;
                    let body: Vec<u8> = chunks
                        .into_iter()
                        .flat_map(|c| c.map(|c| c.to_vec()).unwrap_or_default())
                        .collect();
                    sent.lock()
                        .push(serde_json::from_slice(&body).unwrap_or_default());
                    let reply = serde_json::json!({
                        "id": "chatcmpl-1", "object": "chat.completion", "created": 1, "model": "m",
                        "choices": [{"index": 0, "finish_reason": "stop",
                            "message": {"role": "assistant", "content": answer}}],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
                    });
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
        );
        Gateway::new()
            .client::<Input, Output, _>(OpenAiResponses)
            .and_then(|g| g.provider(Provider::new("chat", OpenAiChat, connect)))
            .and_then(|g| g.target("chat"))
            .map(|g| g.plugin(Responses::with_store(Duration::from_secs(60), store)))
            .expect("a gateway")
    }

    async fn respond(gateway: &Gateway, body: Value) -> Value {
        let mut request = http::Request::new(Body::full(body.to_string()));
        *request.method_mut() = http::Method::POST;
        *request.uri_mut() = http::Uri::from_static("/v1/responses");
        let (response, _) = respan_gateway_http::handle(gateway, request, 1 << 20).await;
        let chunks: Vec<_> = response.into_body().collect().await;
        let bytes: Vec<u8> = chunks
            .into_iter()
            .flat_map(|c| c.map(|c| c.to_vec()).unwrap_or_default())
            .collect();
        gateway.idle().await;
        serde_json::from_slice(&bytes).unwrap_or_default()
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
        let sent = sent.lock();
        let said: Vec<&str> = (sent[1]["messages"].as_array().expect("messages").iter())
            .filter_map(|m| m["content"].as_str())
            .collect();
        assert_eq!(said, ["capital of France?", "Paris", "its population?"]);
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
