//! What vendors of Chat Completions share in their dialects: reasoning
//! taken back as `reasoning_content`, where every vendor that reasons over
//! Chat Completions says it, and OpenAI's own spec has no field. The format
//! reads it in an answer, and writes it to its client; a call to a vendor
//! that takes it back writes it with [`write_reasoning`].

use respan_gateway_conversation::{Content, Input, Message, Role};
use respan_gateway_conversation_http::Schema;
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::Result;

/// Write each assistant message's reasoning as its `reasoning_content`, as
/// a filter of a call's input ([`Dialect::inputs`]).
///
/// [`Dialect::inputs`]: crate::Dialect::inputs
pub fn write_reasoning(input: &mut Input) -> Result<()> {
    input.messages.iter_mut().for_each(write_message_reasoning);
    Ok(())
}

/// The places of `message`'s reasoning but signed reasoning (a signature
/// after it), which the format drops.
fn plain(message: &Message) -> Vec<usize> {
    let content = &message.content;
    let plain = |at: &usize| {
        matches!(content[*at], Content::Reasoning { .. })
            && !matches!(content.get(at + 1), Some(Content::Signature(_)))
    };
    (0..content.len()).filter(plain).collect()
}

/// An assistant message's reasoning, but signed reasoning, which the format
/// drops, as its native `reasoning_content`.
fn write_message_reasoning(message: &mut Message) {
    let plain = plain(message);
    if !matches!(message.role, Role::Assistant) || plain.is_empty() {
        return;
    }
    let mut text = String::new();
    for at in plain.into_iter().rev() {
        if let Content::Reasoning { text: said, .. } = message.content.remove(at) {
            text.insert_str(0, &said);
        }
    }
    let fields = crate::own(&mut message.native, OpenAiChat::NAME);
    fields.insert("reasoning_content".into(), text.into());
}
