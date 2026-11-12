//! Cerebras: Chat Completions.

use respan_gateway_conversation::{Input, Output};
use respan_gateway_conversation_http::Schema;
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::{Filter, Result};

use respan_gateway_vendor_kit::{Dialect, Dialected, Vendor, chat};

/// Where its API is.
const BASE_URL: &str = "https://api.cerebras.ai/v1";

/// Cerebras.
pub const VENDOR: Vendor = Vendor {
    id: "cerebras",
    key_env: &["CEREBRAS_API_KEY"],
    formats: &[(&Dialected::new(OpenAiChat, Cerebras), Some(BASE_URL))],
    ..Vendor::DEFAULT
};

/// Cerebras's Chat Completions: it takes reasoning back as `reasoning`,
/// where most vendors take `reasoning_content`.
#[derive(Debug, Clone, Copy)]
pub struct Cerebras;

impl Dialect for Cerebras {
    type Input = Input;
    type Output = Output;

    fn inputs(&self) -> Option<Box<dyn Filter<Input>>> {
        Some(Box::new(write_reasoning))
    }
}

/// Each assistant message's reasoning as its `reasoning`, as Cerebras
/// takes it back: the kind's, and a client's own `reasoning_content`.
fn write_reasoning(input: &mut Input) -> Result<()> {
    chat::write_reasoning(input)?;
    for message in &mut input.messages {
        if OpenAiChat::fields(&message.native).contains_key("reasoning_content") {
            message.native = OpenAiChat::with_fields(&message.native, |fields| {
                let text = fields.shift_remove("reasoning_content");
                fields.extend(text.map(|text| ("reasoning".into(), text)));
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use respan_gateway_conversation::{Content, Message, Role};
    use respan_gateway_conversation_http::responses::OpenAiResponses;
    use respan_gateway_core::Native;
    use serde_json::{Map, Value, json};

    use super::*;

    #[test]
    fn its_reasoning_is_written_back_as_reasoning() {
        // Responses' fields on its items do not keep their reasoning back.
        let id = |f: &mut Map<String, Value>| _ = f.insert("id".into(), "rs_1".into());
        let responses = OpenAiResponses::with_fields(&Native::default(), id);
        let mut message = Message::new(Role::Assistant, "ok");
        message.native = responses.clone();
        let reasoning = Content::Reasoning {
            text: "hm".into(),
            native: responses,
        };
        message.content.insert(0, reasoning);
        let mut input = Input::default();
        input.messages.push(message);
        write_reasoning(&mut input).expect("written");
        let fields = OpenAiChat::fields(&input.messages[0].native);
        assert_eq!(fields.get("reasoning"), Some(&json!("hm")));
        assert_eq!(fields.get("reasoning_content"), None);
        assert_eq!(fields.get("id"), None);
    }

    #[test]
    fn a_clients_reasoning_content_is_sent_as_reasoning() {
        let body = json!({ "model": "m", "messages": [
            { "role": "user", "content": "hi" },
            { "role": "assistant", "content": "ok", "reasoning_content": "hm" },
        ]});
        let mut input = OpenAiChat.decode_json(&body);
        write_reasoning(&mut input).expect("written");
        let sent = OpenAiChat.encode_request(&input).map(Value::from);
        let message = sent.map(|v| v["messages"][1].clone()).ok();
        let reasoning = json!({ "role": "assistant", "content": "ok", "reasoning": "hm" });
        assert_eq!(message, Some(reasoning));
    }
}
