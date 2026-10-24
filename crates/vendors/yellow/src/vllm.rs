//! vLLM: Chat Completions, as a server one runs serves it.

use respan_gateway_conversation::{Input, Output};
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::Filter;

use respan_gateway_vendor_kit::{Dialect, Dialected, Vendor, chat};

/// vLLM. It has no base URL of its own, and takes a key only if its server
/// was started with one.
pub const VENDOR: Vendor = Vendor {
    id: "vllm",
    key_env: &[],
    formats: &[(&Dialected::new(OpenAiChat, Vllm), None)],
    ..Vendor::DEFAULT
};

/// vLLM's Chat Completions: it takes reasoning back as
/// `reasoning_content`.
#[derive(Debug, Clone, Copy)]
pub struct Vllm;

impl Dialect for Vllm {
    type Input = Input;
    type Output = Output;

    fn inputs(&self) -> Option<Box<dyn Filter<Input>>> {
        Some(Box::new(chat::write_reasoning))
    }
}
