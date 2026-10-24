//! Azure OpenAI: Chat Completions, at a resource's own URL
//! (`https://<resource>.openai.azure.com/openai/v1`).

use respan_gateway_conversation::{Input, Output};
use respan_gateway_conversation_http::openai::OpenAiChat;

use respan_gateway_vendor_kit::{Dialect, Dialected, KeyIn, Vendor};

/// Azure OpenAI. It has no base URL of its own: a resource has one.
pub const VENDOR: Vendor = Vendor {
    id: "azure",
    key_env: &["AZURE_OPENAI_API_KEY"],
    formats: &[(&Dialected::new(OpenAiChat, Azure), None)],
    ..Vendor::DEFAULT
};

/// Azure's Chat Completions: its key in `api-key`.
#[derive(Debug, Clone, Copy)]
pub struct Azure;

impl Dialect for Azure {
    type Input = Input;
    type Output = Output;

    fn key(&self) -> Option<KeyIn> {
        Some(KeyIn::Header("api-key"))
    }
}
