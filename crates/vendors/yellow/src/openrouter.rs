//! OpenRouter: Chat Completions and Responses, for many vendors' models.

use respan_gateway_conversation::Output;
use respan_gateway_conversation_http::dialect::{Call, Dialect, Dialected};
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_conversation_http::responses::OpenAiResponses;
use respan_gateway_core::Filter;

use respan_gateway_vendor_kit::{Vendor, chat};

/// Where its API is.
const BASE_URL: &str = "https://openrouter.ai/api/v1";

/// OpenRouter. Its models' names are `<vendor>/<model>`: a configured
/// provider named `<vendor>` takes the name first. A request goes in its
/// client's format, Chat Completions for a client of another.
pub const VENDOR: Vendor = Vendor {
    id: "openrouter",
    key_env: &["OPENROUTER_API_KEY"],
    formats: &[
        (&Dialected::new(OpenAiChat, OpenRouter), Some(BASE_URL)),
        (&OpenAiResponses, Some(BASE_URL)),
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

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_conversation_http::Schema;
    use serde_json::json;

    #[test]
    fn it_speaks_chat_first_and_responses_beside_it() {
        let names: Vec<_> = VENDOR.formats.iter().map(|(f, _)| f.name()).collect();
        assert_eq!(names, [OpenAiChat::NAME, OpenAiResponses::NAME]);
        assert!(
            VENDOR
                .formats
                .iter()
                .all(|(_, base)| *base == Some(BASE_URL))
        );
    }

    #[test]
    fn its_responses_usage_reads_with_its_own_fields_beside() {
        let body = json!({
            "id": "gen-1", "object": "response", "created_at": 1, "status": "completed",
            "model": "openai/gpt-5.5-codex",
            "output": [{ "id": "msg_1", "type": "message", "status": "completed", "role": "assistant",
                "content": [{ "type": "output_text", "text": "hi", "annotations": [] }] }],
            "usage": { "input_tokens": 5, "output_tokens": 2, "total_tokens": 7,
                "input_tokens_details": { "cached_tokens": 1 },
                "output_tokens_details": { "reasoning_tokens": 0 },
                "cost": 0.0001, "is_byok": false },
        });
        let mut out = Vec::new();
        OpenAiResponses.decode_body(&body, &mut out).expect("read");
        let usage = out.iter().find_map(|o| match o {
            Output::Usage(u) => Some(*u),
            _ => None,
        });
        assert_eq!(
            usage.map(|u| (u.input_tokens, u.output_tokens)),
            Some((5, 2))
        );
    }
}
