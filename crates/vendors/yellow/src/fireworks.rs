//! Fireworks: Chat Completions and embeddings, as OpenAI serves them, and
//! rerank as Cohere does, its answer's results as `data`.

use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_media_http::cohere::CohereRerank;
use respan_gateway_media_http::openai::OpenAiEmbeddings;

use respan_gateway_vendor_kit::Vendor;

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
    ],
    ..Vendor::DEFAULT
};
