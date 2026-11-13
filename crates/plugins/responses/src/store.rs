//! Where the plugin keeps what it keeps ([`Store`]): in this gateway's
//! memory ([`Memory`]), or wherever a deployment of several gateways shares
//! it.

use std::time::Duration;

use respan_gateway_conversation::{Content, MediaSource, Message, Role};
use respan_gateway_core::{Native, async_trait};

/// What is kept under a key: a response's turn of a conversation (the
/// messages its request added, and the reply) under its id, beside the
/// response it followed; or the reasoning an assistant's tool calls came
/// of, under each call's id.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Turn {
    /// The response it followed.
    pub parent: Option<String>,
    /// Its messages.
    pub messages: Vec<Message>,
}

/// Where turns are kept. A turn is read before the call it is sent on, so a
/// store shared by several gateways may answer in its own time; it is kept as
/// its response ends, so `put` returns at once, such a store writing it after.
#[async_trait]
pub trait Store: Send + Sync + 'static {
    /// Keep `turn` under `key`, in place of any there.
    fn put(&self, key: String, turn: Turn);
    /// The turn under `key`, if it is kept; kept afresh, as it was read.
    async fn get(&self, key: &str) -> Option<Turn>;
}

/// Turns kept in this gateway's memory, each for `keep` after it was last
/// written or read, up to [`Memory::BUDGET`] bytes of them, roughly, the
/// least recently used dropped first: not across a restart, nor shared
/// between gateways.
#[derive(Debug)]
pub struct Memory {
    turns: moka::sync::Cache<String, Turn>,
}

impl Memory {
    /// The bytes of messages kept at most, roughly.
    pub const BUDGET: u64 = 256 << 20;

    /// Turns kept for `keep`.
    pub fn new(keep: Duration) -> Self {
        Self::with_budget(keep, Self::BUDGET)
    }

    fn with_budget(keep: Duration, budget: u64) -> Self {
        let bytes = |_: &String, turn: &Turn| {
            let bytes: usize = turn.messages.iter().map(bytes_of).sum();
            u32::try_from(bytes).unwrap_or(u32::MAX)
        };
        let turns = moka::sync::Cache::builder()
            .time_to_idle(keep)
            .weigher(bytes)
            .max_capacity(budget)
            .eviction_policy(moka::policy::EvictionPolicy::lru())
            .build();
        Self { turns }
    }
}

#[async_trait]
impl Store for Memory {
    fn put(&self, key: String, mut turn: Turn) {
        // WARN: natives are dropped, as a store several gateways share drops them.
        turn.messages.iter_mut().for_each(drop_natives);
        self.turns.insert(key, turn);
    }

    async fn get(&self, key: &str) -> Option<Turn> {
        self.turns.get(key)
    }
}

/// `message` without its natives, which are not kept.
fn drop_natives(message: &mut Message) {
    fn part(content: &mut Content) {
        match content {
            Content::Text { native, .. }
            | Content::Reasoning { native, .. }
            | Content::ToolCall { native, .. }
            | Content::Native(native) => *native = Native::default(),
            Content::Image(media) | Content::Audio(media) | Content::Document(media) => {
                media.native = Native::default();
            }
            Content::ToolResult {
                content, native, ..
            } => {
                *native = Native::default();
                content.iter_mut().for_each(part);
            }
            _ => {}
        }
    }
    if let Role::Native(native) = &mut message.role {
        *native = Native::default();
    }
    message.native = Native::default();
    message.content.iter_mut().for_each(part);
}

/// Roughly the bytes `message` holds.
fn bytes_of(message: &Message) -> usize {
    fn part(content: &Content) -> usize {
        match content {
            Content::Text { text, .. } | Content::Reasoning { text, .. } => text.len(),
            Content::Image(media) | Content::Audio(media) | Content::Document(media) => {
                let source = match &media.source {
                    MediaSource::Bytes(bytes) => bytes.len(),
                    MediaSource::Url(url) => url.len(),
                };
                source + media.mime_type.len() + media.name.as_ref().map_or(0, String::len)
            }
            Content::ToolCall {
                id,
                arguments,
                name,
                ..
            } => id.len() + arguments.len() + name.len(),
            Content::ToolResult { id, content, .. } => {
                id.len() + content.iter().map(part).sum::<usize>()
            }
            _ => 256,
        }
    }
    64 + message.content.iter().map(part).sum::<usize>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_conversation::Media;
    use serde_json::json;

    fn kept(message: Message) -> Option<Message> {
        let memory = Memory::with_budget(Duration::from_secs(60), 1_024);
        let messages = vec![message];
        memory.put(
            "response".into(),
            Turn {
                parent: None,
                messages,
            },
        );
        memory.turns.run_pending_tasks();
        memory
            .turns
            .get("response")
            .map(|turn| turn.messages[0].clone())
    }

    #[test]
    fn natives_are_dropped_when_kept() {
        let big = || Native::new(json!({"opaque": "x".repeat(2_048)}));
        let image = Media::new("image/png", MediaSource::Url("https://x.test/i".into()));
        let parts = |native: &dyn Fn() -> Native| {
            let mut media = image.clone();
            media.native = native();
            let mut call = Content::tool_call("call-1", "lookup", "{}");
            let mut result = Content::tool_result("call-1", vec![Content::from("done")], false);
            let mut text = Content::from("prompt");
            for part in [&mut call, &mut result, &mut text] {
                if let Content::ToolCall { native: n, .. }
                | Content::ToolResult { native: n, .. }
                | Content::Text { native: n, .. } = part
                {
                    *n = native();
                }
            }
            let mut message = Message::new(Role::Native(native()), text);
            message.native = native();
            message
                .content
                .extend([Content::Image(media), call, result]);
            message.content.push(Content::Native(native()));
            message
        };
        assert_eq!(kept(parts(&big)), Some(parts(&Native::default)));
    }

    #[test]
    fn content_exceeding_the_budget_is_not_retained() {
        let message = Message::new(Role::User, "x".repeat(2_048).as_str());
        assert_eq!(kept(message), None);
    }
}
