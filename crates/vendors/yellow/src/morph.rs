//! Morph: Chat Completions as OpenAI serves them, and decisions in System
//! One's contract, at `/systemone`.

use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::Fault;
use respan_gateway_decision::{Input, Output};
use respan_gateway_decision_http::Decisions;
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::path::set_path;

use respan_gateway_vendor_kit::{Dialect, Dialected, Rewrite, Vendor};

/// Where its API is.
const BASE_URL: &str = "https://api.morphllm.com/v1";

/// Morph.
pub const VENDOR: Vendor = Vendor {
    id: "morph",
    key_env: &["MORPH_API_KEY"],
    formats: &[
        (&OpenAiChat, Some(BASE_URL)),
        (&Dialected::new(Decisions, Morph), Some(BASE_URL)),
    ],
    ..Vendor::DEFAULT
};

/// Morph's decisions in System One's contract, which it serves at
/// `/systemone`.
#[derive(Debug, Clone, Copy)]
pub struct Morph;

impl Dialect for Morph {
    type Input = Input;
    type Output = Output;

    fn request(&self, head: &mut RequestHead) -> Result<Option<Rewrite>, Fault> {
        set_path(head, "/systemone")?;
        Ok(None)
    }
}
