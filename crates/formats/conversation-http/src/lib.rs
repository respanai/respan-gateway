#![warn(missing_docs)]
//! The conversation kind over HTTP: JSON requests, JSON or SSE responses.
//! A format supplies its [`Schema`], its names and its JSON both ways; the
//! HTTP around it, on either edge, is shared.
//!
//! - [`openai`]: OpenAI Chat Completions.
//! - [`anthropic`]: Anthropic Messages.
//! - [`responses`]: OpenAI Responses.
//! - [`gemini`]: Google Gemini's `generateContent`.
//! - [`bedrock`]: Amazon Bedrock's Converse.
//!
//! A format is its spec: what a provider sends beyond it is kept native, at
//! the place it appeared, for a plugin to read (each module says where), or
//! for a vendor's [`dialect`] of it.

use serde_json::Value;

pub mod dialect;
pub mod fields;
mod schema;
pub use schema::{Asked, Framing, Schema, StreamResponse, merge_details, read_provider_error};

pub mod anthropic;
pub mod bedrock;
pub mod gemini;
pub mod openai;
pub mod responses;

use anthropic::AnthropicMessages;
use bedrock::BedrockConverse;
use gemini::GeminiGenerate;
use openai::OpenAiChat;
use responses::OpenAiResponses;

/// Each format here, by name, with what it says about its own events
/// ([`Schema::is_inert`]).
type Inertness = (&'static str, fn(&Value) -> bool);

const fn inertness<S: Schema>() -> Inertness {
    (S::NAME, S::is_inert)
}

const FORMATS: [Inertness; 5] = [
    inertness::<OpenAiChat>(),
    inertness::<AnthropicMessages>(),
    inertness::<OpenAiResponses>(),
    inertness::<GeminiGenerate>(),
    inertness::<BedrockConverse>(),
];

/// Whether `raw`, an event of `format`, changes no answer: a block's fields
/// ([`Output::block_fields`]), or what that format says is inert
/// ([`Schema::is_inert`]).
///
/// [`Output::block_fields`]: respan_gateway_conversation::Output::block_fields
fn is_inert(format: &str, raw: &Value) -> bool {
    respan_gateway_conversation::block_fields(raw).is_some()
        || FORMATS
            .iter()
            .any(|(name, inert)| *name == format && inert(raw))
}
