//! OpenRouter: Chat Completions, and rerank as Cohere does, for many
//! vendors' models.

use respan_gateway_conversation::Output;
use respan_gateway_conversation_http::dialect::{Call, Dialect, Dialected};
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::Filter;
use respan_gateway_media_http::cohere::CohereRerank;

use respan_gateway_vendor_kit::{Vendor, chat};

/// Where its API is.
const BASE_URL: &str = "https://openrouter.ai/api/v1";

/// OpenRouter. Its models' names are `<vendor>/<model>`: a configured
/// provider named `<vendor>` takes the name first.
pub const VENDOR: Vendor = Vendor {
    id: "openrouter",
    key_env: &["OPENROUTER_API_KEY"],
    formats: &[
        (&Dialected::new(OpenAiChat, OpenRouter), Some(BASE_URL)),
        (&CohereRerank, Some(BASE_URL)),
    ],
    ..Vendor::DEFAULT
};

/// OpenRouter's Chat Completions: a choice's `native_finish_reason` only
/// describes it.
#[derive(Debug, Clone, Copy)]
pub struct OpenRouter;

impl Dialect for OpenRouter {
    fn output(&self, call: &Call) -> Option<Box<dyn Filter<Output>>> {
        chat::read(call, &["native_finish_reason"], false)
    }
}
