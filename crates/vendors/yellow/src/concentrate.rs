//! Concentrate: Chat Completions and Responses, as OpenAI serves them, and
//! decisions in both contracts: System One at `/systemone`, OpenAI's at
//! `/decisions`.

use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_conversation_http::responses::OpenAiResponses;
use respan_gateway_decision_http::{Decisions, OpenAiDecisions};

use respan_gateway_vendor_kit::decisions::SystemOne;
use respan_gateway_vendor_kit::{Dialected, Vendor};

/// Where its API is.
const BASE_URL: &str = "https://api.concentrate.ai/v1";

/// Concentrate.
pub const VENDOR: Vendor = Vendor {
    id: "concentrate",
    key_env: &["CONCENTRATE_API_KEY"],
    formats: &[
        (&OpenAiChat, Some(BASE_URL)),
        (&OpenAiResponses, Some(BASE_URL)),
        (&Dialected::new(Decisions, SystemOne), Some(BASE_URL)),
        (&OpenAiDecisions::DEFAULT, Some(BASE_URL)),
    ],
    ..Vendor::DEFAULT
};
