//! DeepSeek: Chat Completions and Messages.

use respan_gateway_conversation::{Input, Output};
use respan_gateway_conversation_http::anthropic::AnthropicMessages;
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::Filter;

use respan_gateway_vendor_kit::{Dialect, Dialected, Vendor, chat};

/// Where its API is.
const BASE_URL: &str = "https://api.deepseek.com";
/// Where its Anthropic-compatible Messages API is: the format adds the `/v1`.
const MESSAGES_URL: &str = "https://api.deepseek.com/anthropic";

/// DeepSeek. A chat goes in its client's format, Chat Completions for a
/// client of another: an Anthropic client's tool results, failed or not,
/// reach it as they were sent.
pub const VENDOR: Vendor = Vendor {
    id: "deepseek",
    key_env: &["DEEPSEEK_API_KEY"],
    formats: &[
        (&Dialected::new(OpenAiChat, DeepSeek), Some(BASE_URL)),
        (&AnthropicMessages, Some(MESSAGES_URL)),
    ],
    ..Vendor::DEFAULT
};

/// DeepSeek's Chat Completions: it takes reasoning back as
/// `reasoning_content`.
#[derive(Debug, Clone, Copy)]
pub struct DeepSeek;

impl Dialect for DeepSeek {
    type Input = Input;
    type Output = Output;

    fn inputs(&self) -> Option<Box<dyn Filter<Input>>> {
        Some(Box::new(chat::write_reasoning))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_conversation_http::Schema;

    #[test]
    fn it_speaks_chat_first_and_messages_beside_it() {
        let names: Vec<_> = VENDOR.formats.iter().map(|(f, _)| f.name()).collect();
        assert_eq!(names, [OpenAiChat::NAME, AnthropicMessages::NAME]);
        // Messages' base leaves out the `/v1` its format adds.
        assert_eq!(VENDOR.formats[1].1, Some(MESSAGES_URL));
    }
}
