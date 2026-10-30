//! OpenAI: Chat Completions, Responses, Realtime and Responses over WebSocket, and its
//! media endpoints (images, transcriptions, speech, embeddings). Its Videos
//! API and Sora models were removed on 2026-09-24, with no replacement; the
//! format stays, as what clients send for any provider of video.

use respan_gateway_conversation_http::Schema;
use respan_gateway_conversation_http::dialect::{Dialect, Dialected};
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_conversation_http::responses::OpenAiResponses;
use respan_gateway_media_http::openai::{
    OpenAiEmbeddings, OpenAiImages, OpenAiSpeech, OpenAiTranscriptions,
};
use respan_gateway_realtime_ws::{OpenAiRealtime, ResponsesWs};

use respan_gateway_vendor_kit::{Ask, Model, Vendor};

/// Where its API is.
const BASE_URL: &str = "https://api.openai.com/v1";

/// OpenAI. A chat to a model Chat Completions does not serve well goes to
/// its Responses ([`responses_only`]).
pub const VENDOR: Vendor = Vendor {
    id: "openai",
    key_env: &["OPENAI_API_KEY"],
    formats: &[
        (&Dialected::new(OpenAiChat, OpenAi), Some(BASE_URL)),
        (&OpenAiResponses, Some(BASE_URL)),
        (&OpenAiRealtime, Some(BASE_URL)),
        (&ResponsesWs, Some(BASE_URL)),
        (&OpenAiImages, Some(BASE_URL)),
        (&OpenAiTranscriptions, Some(BASE_URL)),
        (&OpenAiSpeech, Some(BASE_URL)),
        (&OpenAiEmbeddings, Some(BASE_URL)),
    ],
    format_for: Some(format_for),
    ..Vendor::DEFAULT
};

/// OpenAI's Chat Completions: its limit is `max_completion_tokens`, as its
/// reasoning models take no `max_tokens`, whatever the client spelled.
#[derive(Debug, Clone, Copy)]
pub struct OpenAi;

impl Dialect for OpenAi {
    fn respells(&self) -> bool {
        false
    }
}

fn format_for(_: &Ask, model: &Model) -> Option<&'static str> {
    responses_only(&model.id).then_some(OpenAiResponses::NAME)
}

/// The models only Responses serves, by a part of their names.
const RESPONSES_ONLY: &[&str] = &["-pro", "codex", "deep-research", "computer-use"];

/// Whether a chat to `model` goes to Responses: a model only it serves, or
/// gpt-5.4 or later, whose reasoning Chat Completions refuses with function
/// tools (from gpt-5.6 they reason unless told not to), as a tool loop may
/// add.
pub fn responses_only(model: &str) -> bool {
    if RESPONSES_ONLY.iter().any(|p| model.contains(p)) {
        return true;
    }
    let Some(version) = model.strip_prefix("gpt-") else {
        return false;
    };
    let version = version.split('-').next().unwrap_or_default();
    let (major, minor) = version.split_once('.').unwrap_or((version, "0"));
    match (major.parse::<u32>(), minor.parse::<u32>()) {
        (Ok(major), Ok(minor)) => (major, minor) >= (5, 4),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn its_chat_limit_is_spelled_its_own_way() {
        assert!(!OpenAi.respells());
    }
}
