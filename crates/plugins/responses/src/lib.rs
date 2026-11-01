#![warn(missing_docs)]
//! The responses plugin ([`Responses`]): conversation state that a client or
//! a provider of the conversation kind cannot keep, kept by the gateway, in
//! a [`Store`] ([`Memory`] unless told otherwise).
//!
//! - **A Responses client, translated to a provider that keeps nothing.**
//!   Its `previous_response_id` works: a response is given an id the
//!   gateway mints (`resp_…`, random, so no provider's ids collide or are
//!   guessed) and kept as its turn (the messages its request added, and the
//!   reply) beside the response it followed; a follow-up naming it is sent
//!   the conversation, read back by following them. A request that says
//!   `store: false` is not kept, and OpenAI's own Responses keep theirs.
//! - **A client of another format, such as Chat Completions, whose call
//!   goes to Responses.** Such a call stores nothing, and its reasoning
//!   comes encrypted (`openai-responses` asks so); the reasoning an
//!   assistant's tool calls came of is kept under each call's id, and put
//!   back before those calls when the client sends them again, as the
//!   client cannot. Without it the model
//!   loses its reasoning between tool calls.
//!
//! What is kept is reached by its id alone, which none can guess.

mod store;

use parking_lot::{Mutex, MutexGuard};
use std::sync::Arc;
use std::time::Duration;

use respan_gateway_conversation::{Assembled, Content, Input, Message, Output, Role};
use respan_gateway_conversation_http::Schema;
use respan_gateway_conversation_http::responses::OpenAiResponses;
use respan_gateway_core::{
    ClientContext, Fault, Filter, NativeFields, Plugin, ProviderContext, Result, async_trait,
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
fn conversation(store: &dyn Store, id: &str) -> Option<Vec<Message>> {
    let mut turns = Vec::new();
    let mut next = Some(id.to_owned());
    while let Some(id) = next.take() {
        let turn = store.get(&id)?;
        if turns.len() == MAX_DEPTH {
            return None;
        }
        next = turn.parent.clone();
        turns.push(turn);
    }
    Some(turns.into_iter().rev().flat_map(|t| t.messages).collect())
}

/// The key a tool call's reasoning is kept under.
fn call_key(id: &str) -> String {
    format!("call:{id}")
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
        if !px.is_kind::<Input>() {
            return Ok(());
        }
        if px.client().format() != RESPONSES {
            return self.carry_reasoning(px);
        }
        let Some(pending) = px.extensions().get::<Pending>().cloned() else {
            return Ok(());
        };
        let (store, keep, target) = (self.store.clone(), self.keep, px.format().to_owned());
        // OpenAI's own Responses keep its responses, but not the gateway's:
        // a conversation that crossed to another provider (a fallback's) is
        // sent whole, as to one that keeps nothing.
        if px.format() == RESPONSES {
            pending.lock().openai = true;
            return px.filter(move |input: &mut Input| {
                let ours = input.native.as_mut().filter(|n| &*n.format == RESPONSES);
                let Some(native) = ours else { return Ok(()) };
                let parent = native
                    .fields
                    .get("previous_response_id")
                    .and_then(Value::as_str);
                let Some(before) = parent.and_then(|id| conversation(&*store, id)) else {
                    return Ok(());
                };
                native.fields.shift_remove("previous_response_id");
                let at = instructions(&pending, input);
                let before = before.into_iter().map(|m| writable(m, &target));
                input.messages.splice(at..at, before);
                Ok(())
            });
        }
        px.filter(move |input: &mut Input| {
            let ours = input.native.as_mut().filter(|n| &*n.format == RESPONSES);
            let (parent, kept) = match ours {
                Some(native) => {
                    let parent = native.fields.shift_remove("previous_response_id");
                    let parent = parent.and_then(|p| p.as_str().map(str::to_owned));
                    (parent, keeps(native))
                }
                None => (None, true),
            };
            let instructions = instructions(&pending, input);
            let added = input.messages[instructions..].to_vec();
            if let Some(id) = &parent {
                let before = conversation(&*store, id).ok_or_else(|| {
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

/// Whether a request's own fields let its response be kept: `store: false`
/// does not.
fn keeps(native: &NativeFields) -> bool {
    native.fields.get("store") != Some(&Value::Bool(false))
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

impl Responses {
    /// A call of a client of another format than Responses: Responses, if
    /// it is the one to write it, puts back the reasoning kept for its tool
    /// calls; and the reasoning this call's answer gives before its tool
    /// calls is kept under their ids.
    fn carry_reasoning(&self, px: &mut ProviderContext) -> Result<()> {
        // Only Responses writes, and answers with, its own reasoning.
        if px.format() != RESPONSES {
            return Ok(());
        }
        let store = self.store.clone();
        px.filter(move |input: &mut Input| {
            put_back(input, |id| {
                let turn = store.get(&call_key(id))?;
                Some(turn.messages.into_iter().flat_map(|m| m.content).collect())
            });
            Ok(())
        })?;
        px.filter(KeepReasoning {
            store: self.store.clone(),
            said: Assembled::default(),
        })
    }
}

/// `input` with the reasoning `earlier` finds for each assistant message
/// without any put back before its tool calls, by its first call's id.
fn put_back(input: &mut Input, earlier: impl Fn(&str) -> Option<Vec<Content>>) {
    for message in &mut input.messages {
        let reasons = message
            .content
            .iter()
            .any(|p| matches!(p, Content::Reasoning { .. }));
        if message.role != Role::Assistant || reasons {
            continue;
        }
        let first_call = message
            .content
            .iter()
            .enumerate()
            .find_map(|(at, p)| match p {
                Content::ToolCall { id, .. } => Some((at, id)),
                _ => None,
            });
        let Some((at, id)) = first_call else {
            continue;
        };
        if let Some(reasoning) = earlier(id) {
            message.content.splice(at..at, reasoning);
        }
    }
}

/// `message` without what `format` cannot write: reasoning or a native part
/// of another format, which only its own provider reads back.
fn writable(mut message: Message, format: &str) -> Message {
    message.content.retain(|part| match part {
        Content::Reasoning { native, .. } => native.as_ref().is_some_and(|n| &*n.format == format),
        Content::Native(native) => &*native.format == format,
        _ => true,
    });
    message
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

/// The reasoning a call's answer gives before its tool calls, kept under
/// each call's id once the answer ended well: Responses' own, as only it
/// reads it back.
struct KeepReasoning {
    store: Arc<dyn Store>,
    said: Assembled,
}

impl Filter<Output> for KeepReasoning {
    fn edit(&mut self, output: Output, out: &mut Vec<Output>) -> Result<()> {
        // What cannot be folded is only not kept; the response goes on.
        let _ = self.said.push(&output);
        out.push(output);
        Ok(())
    }

    fn end(&mut self, end: &Result<()>, _out: &mut Vec<Output>) -> Result<()> {
        if end.is_err() {
            return Ok(());
        }
        let mut reasoning = Vec::new();
        for part in std::mem::take(&mut self.said).replay() {
            match part {
                Content::Reasoning { ref native, .. }
                    if native.as_ref().is_some_and(|n| &*n.format == RESPONSES) =>
                {
                    reasoning.push(part);
                }
                Content::ToolCall { ref id, .. } if !reasoning.is_empty() => {
                    let mut message = Message::new(Role::Assistant, "");
                    message.content = reasoning.clone();
                    let turn = Turn {
                        parent: None,
                        messages: vec![message],
                    };
                    self.store.put(call_key(id), turn);
                }
                Content::ToolCall { .. } => {}
                _ => reasoning.clear(),
            }
        }
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
    use respan_gateway_http::bytes::Bytes;

    #[test]
    fn a_request_that_says_store_false_is_not_kept() {
        let kept = |body: &'static str| {
            let input = OpenAiResponses
                .decode_request(Bytes::from_static(body.as_bytes()))
                .expect("a request");
            input.native.as_ref().is_none_or(keeps)
        };
        assert!(!kept(r#"{"model": "m", "input": "hi", "store": false}"#));
        assert!(kept(r#"{"model": "m", "input": "hi", "store": true}"#));
        assert!(kept(r#"{"model": "m", "input": "hi"}"#));
    }
}
