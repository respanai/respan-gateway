//! Morph: Chat Completions as OpenAI serves them, and decisions in System
//! One's contract, at `/systemone`.

use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_decision_http::Decisions;

use respan_gateway_vendor_kit::decisions::SystemOne;
use respan_gateway_vendor_kit::{Dialected, Vendor};

/// Where its API is.
const BASE_URL: &str = "https://api.morphllm.com/v1";

/// Morph.
pub const VENDOR: Vendor = Vendor {
    id: "morph",
    key_env: &["MORPH_API_KEY"],
    formats: &[
        (&OpenAiChat, Some(BASE_URL)),
        (&Dialected::new(Decisions, SystemOne), Some(BASE_URL)),
    ],
    ..Vendor::DEFAULT
};
