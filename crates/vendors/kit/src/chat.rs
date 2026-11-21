//! What vendors of Chat Completions share in their dialects: reasoning
//! taken back as `reasoning_content`, where every vendor that reasons over
//! Chat Completions says it, and OpenAI's own spec has no field. The format
//! reads it in an answer, and writes it to its client; a call to a vendor
//! that takes it back writes it with [`write_reasoning`]. OpenAI's models
//! take their limit as `max_completion_tokens`, wherever they are served:
//! [`OpenAi`] is the dialect of every vendor of them. [`in_responses`] asks
//! their [`Generation`] whether a chat goes to Responses.

use respan_gateway_conversation::{Content, Input, Message, Output, Role};
use respan_gateway_conversation_http::Schema;
use respan_gateway_conversation_http::openai::{Generation, OpenAiChat};
use respan_gateway_core::{Fault, Result};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::request::Parts as RequestHead;
use serde_json::{Map, Value};

use crate::{Dialect, Rewrite};

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
    message.native = OpenAiChat::with_fields(&message.native, |fields| {
        fields.insert("reasoning_content".into(), text.into());
    });
}

/// Chat Completions as OpenAI's models take it, at OpenAI or any vendor that
/// serves them: the limit as `max_completion_tokens`, as their reasoning
/// models take no `max_tokens`, whatever the client spelled.
#[derive(Debug, Clone, Copy)]
pub struct OpenAi;

impl Dialect for OpenAi {
    type Input = Input;
    type Output = Output;

    fn request(&self, _: &mut RequestHead) -> Result<Option<Rewrite>> {
        Ok(Some(Box::new(completion_limit)))
    }
}

/// A call's JSON with its `max_tokens` as `max_completion_tokens`.
fn completion_limit(body: Bytes) -> Result<Bytes> {
    if !body.windows(12).any(|w| w == b"\"max_tokens\"") {
        return Ok(body);
    }
    let unwritten = |e| Fault::internal("a call is no JSON object").with_source(e);
    let mut call: Map<String, Value> = serde_json::from_slice(&body).map_err(unwritten)?;
    let Some(limit) = call.shift_remove("max_tokens") else {
        return Ok(body);
    };
    call.entry("max_completion_tokens").or_insert(limit);
    serde_json::to_vec(&call)
        .map(Bytes::from)
        .map_err(unwritten)
}

/// Whether a chat to `model` goes to Responses, as its [`Generation`] says;
/// `tools` is whether it offers any ([`Ask::tools`]).
pub fn in_responses(model: &str, tools: bool) -> bool {
    Generation::of(model).in_responses(tools)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn its_chat_limit_is_max_completion_tokens() {
        let sent = |body: &str| {
            let out = completion_limit(Bytes::from(body.to_owned())).expect("written");
            serde_json::from_slice::<Value>(&out).expect("json")
        };
        let renamed = sent(r#"{"model":"gpt-5-mini","max_tokens":7,"messages":[]}"#);
        assert_eq!(renamed["max_completion_tokens"], 7);
        assert!(renamed.get("max_tokens").is_none());
        let both = sent(r#"{"max_tokens":7,"max_completion_tokens":7}"#);
        assert_eq!(both, serde_json::json!({ "max_completion_tokens": 7 }));
        let body = Bytes::from_static(br#"{"max_completion_tokens":7}"#);
        assert_eq!(completion_limit(body.clone()).ok(), Some(body));
    }
}
