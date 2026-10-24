//! DeepSeek: Chat Completions.

use respan_gateway_conversation::{Input, Output};
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::Filter;

use respan_gateway_vendor_kit::{Dialect, Dialected, Vendor, chat};

/// Where its API is.
const BASE_URL: &str = "https://api.deepseek.com";

/// DeepSeek.
pub const VENDOR: Vendor = Vendor {
    id: "deepseek",
    key_env: &["DEEPSEEK_API_KEY"],
    formats: &[(&Dialected::new(OpenAiChat, DeepSeek), Some(BASE_URL))],
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
