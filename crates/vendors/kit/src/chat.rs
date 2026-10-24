//! What vendors of Chat Completions share in their dialects: reasoning
//! taken back as `reasoning_content`, where every vendor that reasons over
//! Chat Completions says it, and OpenAI's own spec has no field. The format
//! reads it in an answer, and writes it to its client; a call to a vendor
//! that takes it back writes it with [`write_reasoning`].

use respan_gateway_conversation::{Content, Input, Message, Role};
use respan_gateway_conversation_http::Schema;
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::{NativeFields, Result};
use serde_json::Map;

/// Write each assistant message's reasoning as its `reasoning_content`, as
/// a filter of a call's input ([`Dialect::inputs`]).
///
/// [`Dialect::inputs`]: crate::Dialect::inputs
pub fn write_reasoning(input: &mut Input) -> Result<()> {
    input.messages.iter_mut().for_each(write_message_reasoning);
    Ok(())
}

/// Reasoning but signed reasoning, which the format refuses.
fn plain(p: &Content) -> bool {
    matches!(p, Content::Reasoning { native, .. } if native.as_ref().is_none_or(NativeFields::is_empty))
}

/// Whether `message` is an assistant's with reasoning to write as its
/// `reasoning_content`.
fn has_reasoning(message: &Message) -> bool {
    let own = message
        .native
        .as_ref()
        .is_none_or(|n| n.is_empty() || &*n.format == OpenAiChat::NAME);
    matches!(message.role, Role::Assistant) && own && message.content.iter().any(plain)
}

/// An assistant message's reasoning, but signed reasoning, which the format
/// refuses, as its native `reasoning_content`.
fn write_message_reasoning(message: &mut Message) {
    if !has_reasoning(message) {
        return;
    }
    let text: String = message
        .content
        .iter()
        .filter(|p| plain(p))
        .filter_map(|p| match p {
            Content::Reasoning { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    message.content.retain(|p| !plain(p));
    let native = message
        .native
        .get_or_insert_with(|| NativeFields::new(OpenAiChat::NAME, Map::new()));
    native.format = OpenAiChat::NAME.into();
    native
        .fields
        .insert("reasoning_content".into(), text.into());
}
