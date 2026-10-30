//! Azure OpenAI: Chat Completions, at a resource's own URL
//! (`https://<resource>.openai.azure.com/openai/v1`).

use respan_gateway_conversation::Output;
use respan_gateway_conversation_http::dialect::{Call, Dialect, Dialected};
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::Filter;

use respan_gateway_vendor_kit::{KeyIn, Vendor, chat};

/// Azure OpenAI. It has no base URL of its own: a resource has one.
pub const VENDOR: Vendor = Vendor {
    id: "azure",
    key_env: &["AZURE_OPENAI_API_KEY"],
    formats: &[(&Dialected::new(OpenAiChat, Azure), None)],
    ..Vendor::DEFAULT
};

/// Azure's Chat Completions: its key in `api-key`, its limit
/// `max_completion_tokens`, as its reasoning models take no `max_tokens`,
/// and a choice's moderation results only describe it.
#[derive(Debug, Clone, Copy)]
pub struct Azure;

impl Dialect for Azure {
    fn output(&self, call: &Call) -> Option<Box<dyn Filter<Output>>> {
        chat::read(
            call,
            &["content_filter_results", "content_filter_offsets"],
            false,
        )
    }

    fn key(&self) -> Option<KeyIn> {
        Some(KeyIn::Header("api-key"))
    }

    fn respells(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deepseek::DeepSeek;
    use crate::openrouter::OpenRouter;

    #[test]
    fn its_limit_is_spelled_its_own_way_as_a_compatible_servers_is_not() {
        assert!(!Azure.respells());
        assert!(DeepSeek.respells() && OpenRouter.respells());
    }
}
