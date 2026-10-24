//! Together: Chat Completions, images, embeddings and rerank.

use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_media_http::cohere::CohereRerank;
use respan_gateway_media_http::openai::{OpenAiEmbeddings, OpenAiImages};

use respan_gateway_vendor_kit::Vendor;

/// Where its API is.
const BASE_URL: &str = "https://api.together.xyz/v1";

/// Together.
pub const VENDOR: Vendor = Vendor {
    id: "together",
    key_env: &["TOGETHER_API_KEY"],
    formats: &[
        (&OpenAiChat, Some(BASE_URL)),
        (&OpenAiImages, Some(BASE_URL)),
        (&OpenAiEmbeddings, Some(BASE_URL)),
        (&CohereRerank, Some(BASE_URL)),
    ],
    ..Vendor::DEFAULT
};
