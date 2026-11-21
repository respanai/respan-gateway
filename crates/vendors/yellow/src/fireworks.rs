//! Fireworks: Chat Completions and embeddings, as OpenAI serves them, rerank
//! as Cohere does, and decisions in both contracts: System One at
//! `/systemone`, OpenAI's at `/decisions`.

use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_decision_http::{Decisions, OpenAiDecisions};
use respan_gateway_media_http::cohere::CohereRerank;
use respan_gateway_media_http::openai::OpenAiEmbeddings;

use respan_gateway_vendor_kit::decisions::SystemOne;
use respan_gateway_vendor_kit::{Dialected, Vendor};

/// Where its API is.
const BASE_URL: &str = "https://api.fireworks.ai/inference/v1";

/// Fireworks.
pub const VENDOR: Vendor = Vendor {
    id: "fireworks",
    key_env: &["FIREWORKS_API_KEY"],
    formats: &[
        (&OpenAiChat, Some(BASE_URL)),
        (&OpenAiEmbeddings, Some(BASE_URL)),
        (&CohereRerank, Some(BASE_URL)),
        (&Dialected::new(Decisions, SystemOne), Some(BASE_URL)),
        (&OpenAiDecisions::DEFAULT, Some(BASE_URL)),
    ],
    ..Vendor::DEFAULT
};
