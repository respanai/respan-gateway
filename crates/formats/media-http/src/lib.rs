#![warn(missing_docs)]
//! The media kinds over HTTP:
//!
//! - [`openai`]: OpenAI's media endpoints, as OpenAI and compatible vendors
//!   serve them, one format per kind.
//! - [`gemini`]: Google's Veo, image and embedding models, as the Gemini
//!   API serves them, of the video, image and embedding kinds.
//! - [`bedrock`]: Amazon Bedrock's embedding and Stability AI image models,
//!   on InvokeModel.
//! - [`minimax`]: MiniMax's video generation and speech synthesis, on its
//!   own API.
//! - [`openrouter`]: OpenRouter's video generation.
//! - [`dashscope`]: Alibaba Cloud Model Studio's speech synthesis,
//!   recognition, image generation and video generation, on DashScope's own
//!   API.
//! - [`together`]: Together's video generation.
//! - [`empiriolabs`]: EmpirioLabs AI's video generation.
//! - [`zai`]: Z.ai's video generation.
//!
//! A vendor's departures from one of them are its dialect of it, laid over
//! it ([`respan_gateway_http::dialect`]).

use respan_gateway_core::Native;
use respan_gateway_http::native::Json;
use serde_json::{Map, Value};

pub mod bedrock;
pub mod dashscope;
mod edge;
pub mod empiriolabs;
pub mod gemini;
pub mod minimax;
pub mod openai;
pub mod openrouter;
pub mod together;
pub mod zai;

pub use edge::{dimensions, nearest_aspect};

/// What each video format of this crate starts its jobs' ids with, by the
/// format's name: its provider's own spelling, or, where its provider's ids
/// are bare, the format's.
pub const JOB_IDS: &[(&str, &str)] = &[
    (openai::OpenAiVideos::NAME, "video_"),
    (gemini::GeminiVeo::NAME, gemini::veo::PREFIX),
    (openrouter::OpenRouterVideos::NAME, "gen-vid-"),
    (together::TogetherVideos::NAME, together::PREFIX),
    (empiriolabs::EmpirioLabsVideos::NAME, empiriolabs::PREFIX),
    (dashscope::DashScopeVideos::NAME, dashscope::videos::PREFIX),
    (zai::ZaiVideos::NAME, zai::PREFIX),
    (minimax::MiniMaxVideos::NAME, minimax::videos::PREFIX),
];

/// What the video format named `format` starts its jobs' ids with, which
/// routes a follow-up to the job's provider.
pub fn job_ids(format: &str) -> Option<&'static str> {
    let known = JOB_IDS.iter().find(|(name, _)| *name == format);
    known.map(|(_, ids)| *ids)
}

/// Whether `id` is spelled as a job id a video format of this crate gives
/// out, perhaps wrapped in its provider's name (`<provider>~<id>`).
pub(crate) fn issued(id: &str) -> bool {
    let inner = id.split_once('~').map_or(id, |(_, inner)| inner);
    JOB_IDS.iter().any(|(_, ids)| inner.starts_with(ids))
}

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
impl Natives for empiriolabs::EmpirioLabsVideos {}
impl Natives for gemini::GeminiEmbeddings {}
impl Natives for gemini::GeminiImages {}
impl Natives for gemini::GeminiSpeech {}
impl Natives for gemini::GeminiVeo {}
impl Natives for minimax::MiniMaxVideos {}
impl Natives for openrouter::OpenRouterVideos {}
impl Natives for dashscope::DashScopeImages {}
impl Natives for dashscope::DashScopeSpeech {}
impl Natives for dashscope::DashScopeTranscriptions {}
impl Natives for dashscope::DashScopeVideos {}
impl Natives for minimax::MiniMaxSpeech {}
impl Natives for together::TogetherVideos {}
impl Natives for zai::ZaiVideos {}
