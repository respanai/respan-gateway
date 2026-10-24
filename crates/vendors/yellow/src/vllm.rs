//! vLLM: Chat Completions, and rerank as Cohere does, as a server one runs
//! serves them.

use std::borrow::Cow;

use respan_gateway_conversation::{Input, Output};
use respan_gateway_conversation_http::dialect::{Call, Dialect, Dialected};
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::{Fault, Filter};
use respan_gateway_media_http::cohere::CohereRerank;

use respan_gateway_vendor_kit::{Vendor, chat};

/// vLLM. It has no base URL of its own, and takes a key only if its server
/// was started with one.
pub const VENDOR: Vendor = Vendor {
    id: "vllm",
    key_env: &[],
    formats: &[
        (&Dialected::new(OpenAiChat, Vllm), None),
        (&CohereRerank, None),
    ],
    ..Vendor::DEFAULT
};

/// vLLM's Chat Completions: reasoning is `reasoning_content`, and a
/// choice's `stop_reason` only describes it.
#[derive(Debug, Clone, Copy)]
pub struct Vllm;

impl Dialect for Vllm {
    fn input<'a>(&self, input: &'a Input, _: &Call) -> Result<Cow<'a, Input>, Fault> {
        Ok(chat::reasoning_written(input))
    }

    fn output(&self, call: &Call) -> Option<Box<dyn Filter<Output>>> {
        chat::read(call, &["stop_reason"], true)
    }
}
