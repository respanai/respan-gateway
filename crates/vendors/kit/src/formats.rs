//! Every spec's format, as a provider of no vendor speaks it ([`FORMATS`]),
//! and a format by name, as a vendor speaks it or as its spec has it
//! ([`format()`]).

use respan_gateway_conversation_http::anthropic::AnthropicMessages;
use respan_gateway_conversation_http::bedrock::BedrockConverse;
use respan_gateway_conversation_http::gemini::GeminiGenerate;
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_conversation_http::responses::OpenAiResponses;
use respan_gateway_decision_http::Decisions;
use respan_gateway_media_http::bedrock::BedrockEmbeddings;
use respan_gateway_media_http::gemini::{GeminiImages, GeminiVeo};
use respan_gateway_media_http::openai::{
    OpenAiEmbeddings, OpenAiImages, OpenAiSpeech, OpenAiTranscriptions, OpenAiVideos,
};
use respan_gateway_passthrough_http::Passthrough;
use respan_gateway_realtime_ws::{GeminiLive, MistralRealtime, OpenAiRealtime, ResponsesWs};

use crate::{Upstream, Vendor};

/// Every spec's format, as a provider of no vendor speaks it: as its spec
/// has it reached ([`Upstream`]), with no departure of a vendor's.
pub const FORMATS: &[&dyn Upstream] = &[
    &OpenAiChat,
    &OpenAiResponses,
    &AnthropicMessages,
    &GeminiGenerate,
    &BedrockConverse,
    &OpenAiRealtime,
    &ResponsesWs,
    &GeminiLive,
    &MistralRealtime,
    &OpenAiImages,
    &OpenAiTranscriptions,
    &OpenAiSpeech,
    &OpenAiEmbeddings,
    &OpenAiVideos,
    &GeminiVeo,
    &GeminiImages,
    &BedrockEmbeddings,
    &Decisions,
    &Passthrough,
];

/// Format `name` as `vendor` speaks it, at its base URL; else as its spec
/// has it ([`FORMATS`]), at none. `None` for a format of neither.
pub fn format(
    vendor: Option<&Vendor>,
    name: &str,
) -> Option<(&'static dyn Upstream, Option<&'static str>)> {
    let own = vendor.and_then(|v| v.format(name));
    let spec = || FORMATS.iter().copied().find(|f| f.name() == name);
    own.or_else(|| spec().map(|f| (f, None)))
}
