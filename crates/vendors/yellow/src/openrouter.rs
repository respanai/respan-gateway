//! OpenRouter: Chat Completions, for many vendors' models.

use respan_gateway_conversation_http::openai::OpenAiChat;

use respan_gateway_vendor_kit::Vendor;

/// Where its API is.
const BASE_URL: &str = "https://openrouter.ai/api/v1";

/// OpenRouter. Its models' names are `<vendor>/<model>`: a configured
/// provider named `<vendor>` takes the name first.
pub const VENDOR: Vendor = Vendor {
    id: "openrouter",
    key_env: &["OPENROUTER_API_KEY"],
    formats: &[(&OpenAiChat, Some(BASE_URL))],
    ..Vendor::DEFAULT
};
