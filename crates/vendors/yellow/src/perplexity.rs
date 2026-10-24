//! Perplexity: its Agent API, as OpenAI's Responses. What only it sends,
//! what the tools it runs itself found and a stream's own events about its
//! reasoning and skills, is native to Responses, so reaches only a client
//! of Responses.

use respan_gateway_conversation_http::responses::OpenAiResponses;

use respan_gateway_vendor_kit::Vendor;

/// Where its API is.
const BASE_URL: &str = "https://api.perplexity.ai/v1";

/// Perplexity. Its models' names are `<vendor>/<model>`: a configured
/// provider named `<vendor>` takes the name first.
pub const VENDOR: Vendor = Vendor {
    id: "perplexity",
    key_env: &["PERPLEXITY_API_KEY"],
    formats: &[(&OpenAiResponses, Some(BASE_URL))],
    ..Vendor::DEFAULT
};
