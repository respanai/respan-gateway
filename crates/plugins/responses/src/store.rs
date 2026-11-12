//! Where the plugin keeps what it keeps ([`Store`]): in this gateway's
//! memory ([`Memory`]), or wherever a deployment of several gateways shares
//! it.

use std::time::Duration;

use respan_gateway_conversation::{Content, MediaSource, Message};
use respan_gateway_core::async_trait;

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
    fn put(&self, key: String, turn: Turn) {
        self.turns.insert(key, turn);
    }

    async fn get(&self, key: &str) -> Option<Turn> {
        self.turns.get(key)
    }
}

/// Roughly the bytes `message` holds, including format-specific data.
fn bytes_of(message: &Message) -> usize {
    fn part(content: &Content) -> usize {
        match content {
            Content::Text { text, native, .. } | Content::Reasoning { text, native } => {
                text.len() + native.len()
            }
            Content::Image(media) | Content::Audio(media) | Content::Document(media) => {
                let source = match &media.source {
                    MediaSource::Bytes(bytes) => bytes.len(),
                    MediaSource::Url(url) => url.len(),
                };
                source
                    + media.mime_type.len()
                    + media.name.as_ref().map_or(0, String::len)
                    + media.native.len()
            }
            Content::ToolCall {
                id,
                arguments,
                name,
                native,
                ..
            } => id.len() + arguments.len() + name.len() + native.len(),
            Content::ToolResult {
                id,
                content,
                native,
                ..
            } => id.len() + content.iter().map(part).sum::<usize>() + native.len(),
            Content::Native(native) => native.len(),
            _ => 256,
        }
    }
    64 + message.content.iter().map(part).sum::<usize>() + message.native.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_conversation::{Media, Role};
    use respan_gateway_conversation_http::Schema;
    use respan_gateway_conversation_http::responses::OpenAiResponses;
    use serde_json::json;

    fn retained(message: Message) -> bool {
        let memory = Memory::with_budget(Duration::from_secs(60), 1_024);
        memory.put(
            "response".into(),
            Turn {
                parent: None,
                messages: vec![message],
            },
        );
        memory.turns.run_pending_tasks();
        memory.turns.get("response").is_some()
    }

    #[test]
    fn native_content_exceeding_the_budget_is_not_retained() {
        let message = Message::new(
            Role::User,
            Content::Native(OpenAiResponses::native(
                json!({"opaque": "x".repeat(2_048)}),
            )),
        );
        assert!(!retained(message));
    }

    #[test]
    fn native_text_fields_exceeding_the_budget_are_not_retained() {
        let mut message = Message::new(Role::User, "prompt");
        let Content::Text { native, .. } = &mut message.content[0] else {
            unreachable!()
        };
        *native = OpenAiResponses::native(json!({"annotation": "x".repeat(2_048)}));
        assert!(!retained(message));
    }

    #[test]
    fn native_message_fields_exceeding_the_budget_are_not_retained() {
        let mut message = Message::new(Role::User, "prompt");
        message.native =
            OpenAiResponses::native(json!({"metadata": {"payload": "x".repeat(2_048)}}));
        assert!(!retained(message));
    }

    #[test]
    fn native_media_and_tool_fields_exceeding_the_budget_are_not_retained() {
        let native = OpenAiResponses::native(json!({"payload": "x".repeat(2_048)}));

        let mut media = Media::new("image/png", MediaSource::Url("https://x.test/i".into()));
        media.native = native.clone();
        assert!(!retained(Message::new(Role::User, Content::Image(media))));

        let mut call = Content::tool_call("call-1", "lookup", "{}");
        if let Content::ToolCall { native: fields, .. } = &mut call {
            *fields = native.clone();
        }
        assert!(!retained(Message::new(Role::Assistant, call)));

        let mut result = Content::tool_result("call-1", vec![Content::from("done")], false);
        if let Content::ToolResult { native: fields, .. } = &mut result {
            *fields = native;
        }
        assert!(!retained(Message::new(Role::User, result)));
    }

    #[test]
    fn small_native_content_is_retained() {
        let message = Message::new(
            Role::User,
            Content::Native(OpenAiResponses::native(json!({"opaque": "small"}))),
        );
        assert!(retained(message));
    }
}
