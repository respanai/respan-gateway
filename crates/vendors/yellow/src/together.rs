//! Together: Chat Completions, images and embeddings, as OpenAI serves them,
//! rerank as Cohere does, and videos.

use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_media_http::cohere::CohereRerank;
use respan_gateway_media_http::openai::{OpenAiEmbeddings, OpenAiImages};
use respan_gateway_media_http::together::TogetherVideos;

use respan_gateway_vendor_kit::Vendor;

/// Where its API is.
const BASE_URL: &str = "https://api.together.xyz/v1";

/// Where its videos are: the format adds the `/v2`.
const VIDEOS_URL: &str = "https://api.together.xyz";

/// Together.
pub const VENDOR: Vendor = Vendor {
    id: "together",
    key_env: &["TOGETHER_API_KEY"],
    formats: &[
        (&OpenAiChat, Some(BASE_URL)),
        (&OpenAiImages, Some(BASE_URL)),
        (&OpenAiEmbeddings, Some(BASE_URL)),
        (&CohereRerank, Some(BASE_URL)),
        (&TogetherVideos, Some(VIDEOS_URL)),
    ],
    ..Vendor::DEFAULT
};
