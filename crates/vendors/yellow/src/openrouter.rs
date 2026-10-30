//! OpenRouter: Chat Completions and Responses, for many vendors' models.

use std::borrow::Cow;

use respan_gateway_conversation::{Input, Output};
use respan_gateway_conversation_http::Schema;
use respan_gateway_conversation_http::dialect::{Call, Dialect, Dialected};
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_conversation_http::responses::OpenAiResponses;
use respan_gateway_core::{Filter, NativeFields, Result};
use serde_json::{Map, json};

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

/// OpenRouter's Chat Completions: a thinking budget, such as Anthropic's
/// clients send, is its `reasoning.max_tokens`; a choice's
/// `native_finish_reason` only describes it.
#[derive(Debug, Clone, Copy)]
pub struct OpenRouter;

impl Dialect for OpenRouter {
    fn input<'a>(&self, input: &'a Input, _: &Call) -> Result<Cow<'a, Input>> {
        let budget = input
            .options
            .reasoning
            .as_ref()
            .and_then(|r| r.budget_tokens);
        // Another format's fields leave no room for its own.
        let own = input
            .native
            .as_ref()
            .is_none_or(|n| n.is_empty() || &*n.format == OpenAiChat::NAME);
        let (Some(budget), true) = (budget, own) else {
            return Ok(Cow::Borrowed(input));
        };
        let mut input = input.clone();
        let withheld = input.options.reasoning.take().and_then(|r| r.summary) == Some(false);
        let mut reasoning = json!({ "max_tokens": budget });
        if withheld {
            reasoning["exclude"] = true.into();
        }
        let native = input
            .native
            .get_or_insert_with(|| NativeFields::new(OpenAiChat::NAME, Map::new()));
        native.format = OpenAiChat::NAME.into();
        native.fields.entry("reasoning").or_insert(reasoning);
        Ok(Cow::Owned(input))
    }

    fn output(&self, call: &Call) -> Option<Box<dyn Filter<Output>>> {
        chat::read(call, &["native_finish_reason"], false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_conversation_http::anthropic::AnthropicMessages;

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

    #[test]
    fn a_thinking_budget_is_its_reasoning_limit() {
        let messages = AnthropicMessages.decode_json(&json!({
            "max_tokens": 9000,
            "messages": [{ "role": "user", "content": "hi" }],
            "thinking": { "type": "enabled", "budget_tokens": 2048 },
        }));
        let call = Call::new;
        let taken = OpenRouter.input(&messages, &call(true)).expect("taken");
        assert!(taken.options.reasoning.is_none());
        let sent = OpenAiChat.encode_request(&taken).expect("written");
        assert_eq!(sent["reasoning"], json!({ "max_tokens": 2048 }));
        assert!(sent.get("reasoning_effort").is_none());
        // Without a budget the input is as it came.
        let plain = AnthropicMessages.decode_json(&json!({
            "max_tokens": 9,
            "messages": [{ "role": "user", "content": "hi" }],
        }));
        assert!(matches!(
            OpenRouter.input(&plain, &call(true)),
            Ok(Cow::Borrowed(_))
        ));
    }
}
