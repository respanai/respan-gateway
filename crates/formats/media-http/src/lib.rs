#![warn(missing_docs)]
//! The media kinds over HTTP:
//!
//! - [`openai`]: OpenAI's media endpoints, as OpenAI and compatible vendors
//!   serve them, one format per kind.
//! - [`gemini`]: Google's Veo and image models, as the Gemini API serves
//!   them, of the video and image kinds.
//! - [`bedrock`]: Amazon Bedrock's embedding models, on InvokeModel.
//! - [`openrouter`]: OpenRouter's video generation.
//!
//! A vendor's departures from one of them are its dialect of it, laid over
//! it ([`respan_gateway_http::dialect`]).

use respan_gateway_core::Native;
use respan_gateway_http::native::Json;
use serde_json::{Map, Value};

pub mod bedrock;
mod edge;
pub mod gemini;
pub mod openai;
pub mod openrouter;

/// What only format `F` of this crate reads of a value: no other crate can name it.
struct Own<F>(std::marker::PhantomData<fn() -> F>);

/// A format's own natives, which code that names the format reaches.
pub trait Natives: Sized + 'static {
    /// Its own fields in `native`: none if it holds another format's.
    fn fields(native: &Native) -> &Map<String, Value> {
        native.fields::<Own<Self>>()
    }
    /// Its own fields in `native`, edited by `edit`: another format's dropped.
    fn with_fields(native: &Native, edit: impl FnOnce(&mut Map<String, Value>)) -> Native {
        native.with_fields::<Own<Self>>(edit)
    }
}
impl Natives for openai::OpenAiImages {}
impl Natives for openai::OpenAiEmbeddings {}
impl Natives for openai::OpenAiSpeech {}
impl Natives for openai::OpenAiTranscriptions {}
impl Natives for openai::OpenAiVideos {}
impl Natives for gemini::GeminiImages {}
impl Natives for gemini::GeminiVeo {}
impl Natives for openrouter::OpenRouterVideos {}
