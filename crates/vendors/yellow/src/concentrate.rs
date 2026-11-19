//! Concentrate: Chat Completions and Responses, as OpenAI serves them, and
//! decisions in both contracts: System One at `/systemone`, OpenAI's at
//! `/decisions`.

use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_conversation_http::responses::OpenAiResponses;
use respan_gateway_core::Fault;
use respan_gateway_decision::{Input, Output};
use respan_gateway_decision_http::{Decisions, OpenAiDecisions};
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::path::set_path;

use respan_gateway_vendor_kit::{Dialect, Dialected, Rewrite, Vendor};

/// Where its API is.
const BASE_URL: &str = "https://api.concentrate.ai/v1";

/// Concentrate.
pub const VENDOR: Vendor = Vendor {
    id: "concentrate",
    key_env: &["CONCENTRATE_API_KEY"],
    formats: &[
        (&OpenAiChat, Some(BASE_URL)),
        (&OpenAiResponses, Some(BASE_URL)),
        (&Dialected::new(Decisions, Concentrate), Some(BASE_URL)),
        (&OpenAiDecisions::DEFAULT, Some(BASE_URL)),
    ],
    ..Vendor::DEFAULT
};

/// Concentrate's decisions in System One's contract, which it serves at
/// `/systemone`.
#[derive(Debug, Clone, Copy)]
pub struct Concentrate;

impl Dialect for Concentrate {
    type Input = Input;
    type Output = Output;

    fn request(&self, head: &mut RequestHead) -> Result<Option<Rewrite>, Fault> {
        set_path(head, "/systemone")?;
        Ok(None)
    }
}
