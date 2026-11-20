//! Azure OpenAI: Chat Completions, decisions and Realtime, at a resource's
//! own URL (`https://<resource>.openai.azure.com/openai/v1`). Its Chat
//! Completions serve OpenAI's models on OpenAI's wire, so it is OpenAI's
//! dialect, its key a Bearer one, as the v1 surface takes it.

use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_realtime as realtime;
use respan_gateway_realtime_ws::OpenAiRealtime;

use respan_gateway_vendor_kit::chat::OpenAi;
use respan_gateway_vendor_kit::{Dialect, Dialected, KeyIn, Vendor};

/// Azure OpenAI. It has no base URL of its own: a resource has one.
pub const VENDOR: Vendor = Vendor {
    id: "azure",
    key_env: &["AZURE_OPENAI_API_KEY"],
    formats: &[
        (&Dialected::new(OpenAiChat, OpenAi), None),
        (
            &Dialected::new(
                respan_gateway_decision_http::OpenAiDecisions::DEFAULT,
                AzureDecisions,
            ),
            None,
        ),
        (&Dialected::new(OpenAiRealtime, AzureRealtime), None),
    ],
    ..Vendor::DEFAULT
};

/// Azure Decisions authentication.
#[derive(Debug, Clone, Copy)]
pub struct AzureDecisions;
impl Dialect for AzureDecisions {
    type Input = respan_gateway_decision::Input;
    type Output = respan_gateway_decision::Output;
    fn key(&self) -> Option<KeyIn> {
        Some(KeyIn::Header("api-key"))
    }
}

/// Azure's Realtime: OpenAI's GA protocol at a resource's
/// `/openai/v1/realtime`, the deployment at `?model=`, its key in `api-key`.
#[derive(Debug, Clone, Copy)]
pub struct AzureRealtime;
impl Dialect for AzureRealtime {
    type Input = realtime::Input;
    type Output = realtime::Output;
    fn key(&self) -> Option<KeyIn> {
        Some(KeyIn::Header("api-key"))
    }
}
