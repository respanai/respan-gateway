//! Cerebras: Chat Completions.

use respan_gateway_conversation::{Block, Delta, Input, Output};
use respan_gateway_conversation_http::Schema;
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::{Filter, Native, Result};
use serde_json::{Map, Value};

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

/// Cerebras's Chat Completions: reasoning is `reasoning`, where most
/// vendors' is `reasoning_content`, in a response and in an assistant
/// message sent back.
#[derive(Debug, Clone, Copy)]
pub struct Cerebras;

impl Dialect for Cerebras {
    type Input = Input;
    type Output = Output;

    fn inputs(&self) -> Option<Box<dyn Filter<Input>>> {
        Some(Box::new(write_reasoning))
    }

    fn outputs(&self) -> Option<Box<dyn Filter<Output>>> {
        Some(Box::<Reasoning>::default())
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

/// Reads a response's `reasoning` as reasoning blocks of its own numbers, counting down from the last, which the format never gives: one
/// opens with the first reasoning and ends before any other block's event,
/// or a finish.
#[derive(Default)]
struct Reasoning {
    open: Option<u32>,
    opened: u32,
}

impl Reasoning {
    fn close(&mut self, out: &mut Vec<Output>) {
        if let Some(block) = self.open.take() {
            out.push(Output::block_end(block));
        }
    }
}

impl Filter<Output> for Reasoning {
    fn edit(&mut self, output: Output, out: &mut Vec<Output>) -> Result<()> {
        match output {
            Output::Native(_) => {
                let mut said = None;
                out.extend(first_choice(output, |choice| {
                    let delta = choice.get_mut("delta").and_then(Value::as_object_mut);
                    let text = delta.and_then(|d| d.shift_remove("reasoning"));
                    said = text.and_then(|t| t.as_str().map(str::to_owned));
                }));
                let Some(text) = said.filter(|t| !t.is_empty()) else {
                    return Ok(());
                };
                let block = match self.open {
                    Some(block) => block,
                    None => {
                        let block = u32::MAX - self.opened;
                        self.opened += 1;
                        self.open = Some(block);
                        out.push(Output::block_start(block, Block::Reasoning));
                        block
                    }
                };
                out.push(Output::Delta {
                    block,
                    delta: Delta::Reasoning(text),
                });
            }
            Output::BlockStart { .. } | Output::BlockEnd { .. } | Output::Finish(_) => {
                self.close(out);
                out.push(output);
            }
            output => out.push(output),
        }
        Ok(())
    }

    fn end(&mut self, end: &Result<()>, out: &mut Vec<Output>) -> Result<()> {
        if end.is_ok() {
            self.close(out);
        }
        Ok(())
    }
}

/// `output` with `edit` made to its first choice, if it is a native of
/// Chat Completions that has one; `None` once nothing is left of it.
fn first_choice(output: Output, edit: impl FnOnce(&mut Map<String, Value>)) -> Option<Output> {
    let mut fields = match &output {
        Output::Native(native) => match OpenAiChat::fields(native) {
            fields if fields.get("choices").is_some_and(Value::is_array) => fields.clone(),
            _ => return Some(output),
        },
        _ => return Some(output),
    };
    let choices = fields.get_mut("choices").and_then(Value::as_array_mut)?;
    let first = choices
        .iter_mut()
        .find(|c| c["index"].as_u64().unwrap_or(0) == 0);
    if let Some(choice) = first.and_then(Value::as_object_mut) {
        edit(choice);
        if choice
            .get("delta")
            .and_then(Value::as_object)
            .is_some_and(Map::is_empty)
        {
            choice.shift_remove("delta");
        }
    }
    choices.retain(|c| c.as_object().is_none_or(|c| c.keys().any(|k| k != "index")));
    if choices.is_empty() && fields.len() == 1 {
        return None;
    }
    Some(Output::Native(chat_native(fields)))
}

/// `fields` as Chat Completions' own.
fn chat_native(fields: Map<String, Value>) -> Native {
    OpenAiChat::with_fields(&Native::default(), |f| *f = fields)
}

#[cfg(test)]
mod tests {
    use respan_gateway_conversation::{Content, Message, Role};
    use respan_gateway_conversation_http::responses::OpenAiResponses;
    use serde_json::json;

    use super::*;

    #[test]
    fn its_reasoning_is_read_and_written_back_as_reasoning() {
        let mut reasoning = Cerebras.outputs().expect("a filter");
        let mut out = Vec::new();
        let delta = json!({ "choices": [{ "index": 0, "delta": { "reasoning": "hm" } }] });
        reasoning
            .edit(
                Output::Native(chat_native(
                    serde_json::from_value(delta).expect("an object"),
                )),
                &mut out,
            )
            .expect("read");
        reasoning
            .edit(
                Output::finished(respan_gateway_conversation::Finish::Stop),
                &mut out,
            )
            .expect("read");
        let block = u32::MAX;
        assert_eq!(
            out,
            [
                Output::block_start(block, Block::Reasoning),
                Output::Delta {
                    block,
                    delta: Delta::Reasoning("hm".into()),
                },
                Output::block_end(block),
                Output::finished(respan_gateway_conversation::Finish::Stop),
            ]
        );
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
