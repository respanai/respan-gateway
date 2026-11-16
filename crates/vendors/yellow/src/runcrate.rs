//! Runcrate: Chat Completions, as OpenAI serves them.

use respan_gateway_conversation_http::openai::OpenAiChat;

use respan_gateway_vendor_kit::Vendor;

/// Where its API is.
const BASE_URL: &str = "https://api.runcrate.ai/v1";

/// Runcrate.
pub const VENDOR: Vendor = Vendor {
    id: "runcrate",
    key_env: &["RUNCRATE_API_KEY"],
    formats: &[(&OpenAiChat, Some(BASE_URL))],
    ..Vendor::DEFAULT
};
