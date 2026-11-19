//! Baseten: Chat Completions and Responses, as OpenAI serves them,
//! Anthropic's Messages, and decisions in System One's contract, at
//! `/systemone`.

use respan_gateway_conversation_http::anthropic::AnthropicMessages;
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_conversation_http::responses::OpenAiResponses;
use respan_gateway_core::Fault;
use respan_gateway_decision::{Input, Output};
use respan_gateway_decision_http::Decisions;
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::path::set_path;

use respan_gateway_vendor_kit::{Dialect, Dialected, KeyIn, Rewrite, Vendor};

/// Where its Model APIs are.
const BASE_URL: &str = "https://inference.baseten.co/v1";

/// Where its Messages API is: the format adds the `/v1`.
const MESSAGES_URL: &str = "https://inference.baseten.co";

/// Baseten. Its decision models are named as it lists them, such as
/// `decision-models/baseten-elephant-9b-latest`.
pub const VENDOR: Vendor = Vendor {
    id: "baseten",
    key_env: &["BASETEN_API_KEY"],
    formats: &[
        (&OpenAiChat, Some(BASE_URL)),
        (&OpenAiResponses, Some(BASE_URL)),
        (
            &Dialected::new(AnthropicMessages, Messages),
            Some(MESSAGES_URL),
        ),
        (&Dialected::new(Decisions, Baseten), Some(BASE_URL)),
    ],
    ..Vendor::DEFAULT
};

/// Anthropic's Messages as Baseten serves them: the key a bearer token.
#[derive(Debug, Clone, Copy)]
pub struct Messages;

impl Dialect for Messages {
    type Input = respan_gateway_conversation::Input;
    type Output = respan_gateway_conversation::Output;

    fn key(&self) -> Option<KeyIn> {
        Some(KeyIn::Bearer)
    }
}

/// Baseten's decisions in System One's contract, which it serves at
/// `/systemone`.
#[derive(Debug, Clone, Copy)]
pub struct Baseten;

impl Dialect for Baseten {
    type Input = Input;
    type Output = Output;

    fn request(&self, head: &mut RequestHead) -> Result<Option<Rewrite>, Fault> {
        set_path(head, "/systemone")?;
        Ok(None)
    }
}
