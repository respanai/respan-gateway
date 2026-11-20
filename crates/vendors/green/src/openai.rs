//! OpenAI: Chat Completions, Responses, Realtime, Responses over WebSocket,
//! its Live API, its media endpoints (images, transcriptions, speech,
//! embeddings), and its decisions. Its Videos API and Sora models were removed on 2026-09-24, with
//! no replacement; the format stays, as what clients send for any provider of
//! video.

use respan_gateway_conversation_http::Schema;
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_conversation_http::responses::OpenAiResponses;
use respan_gateway_decision_http::OpenAiDecisions;
use respan_gateway_media_http::openai::{
    OpenAiEmbeddings, OpenAiImages, OpenAiSpeech, OpenAiTranscriptions,
};
use respan_gateway_realtime_ws::{OpenAiLive, OpenAiRealtime};

use respan_gateway_vendor_kit::chat::OpenAi;
use respan_gateway_vendor_kit::{Ask, Dialected, Model, Vendor};

/// Where its API is.
const BASE_URL: &str = "https://api.openai.com/v1";

/// OpenAI. A chat to a model Chat Completions does not serve well goes to
/// its Responses ([`in_responses`]); a session with a Live model goes to its
/// Live API, and one with any other to Realtime ([`is_live`]).
pub const VENDOR: Vendor = Vendor {
    id: "openai",
    key_env: &["OPENAI_API_KEY"],
    formats: &[
        (&Dialected::new(OpenAiChat, OpenAi), Some(BASE_URL)),
        (&OpenAiResponses, Some(BASE_URL)),
        (&OpenAiRealtime, Some(BASE_URL)),
        (&OpenAiLive, Some(BASE_URL)),
        (&OpenAiImages, Some(BASE_URL)),
        (&OpenAiTranscriptions, Some(BASE_URL)),
        (&OpenAiSpeech, Some(BASE_URL)),
        (&OpenAiEmbeddings, Some(BASE_URL)),
        (&OpenAiDecisions::DEFAULT, Some(BASE_URL)),
    ],
    format_for: Some(format_for),
    ..Vendor::DEFAULT
};

fn format_for(ask: &Ask, model: &Model) -> Option<&'static str> {
    match ask.kind {
        "realtime" if is_live(&model.id) => Some(OpenAiLive::NAME),
        "realtime" => Some(OpenAiRealtime::NAME),
        _ => in_responses(&model.id, ask.tools).then_some(OpenAiResponses::NAME),
    }
}

/// Whether `model` is a Live model, which only its Live API serves, and
/// only it: `gpt-live-` and a version, such as `gpt-live-1`.
/// `gpt-live-transcribe` transcribes for Realtime.
pub fn is_live(model: &str) -> bool {
    model
        .strip_prefix("gpt-live-")
        .is_some_and(|version| version.starts_with(|c: char| c.is_ascii_digit()))
}

/// The models only Responses serves, by a part of their names.
const RESPONSES_ONLY: &[&str] = &["-pro", "codex", "deep-research", "computer-use"];

/// Whether a chat to `model` goes to Responses: a model only it serves, or,
/// if it offers `tools` ([`Ask::tools`]), gpt-5.4 or later, whose reasoning
/// Chat Completions refuses with function tools (from gpt-5.6 they reason
/// unless told not to). Any other chat stays in Chat Completions, which
/// takes what Responses cannot, such as `stop`, `seed` and `n`.
pub fn in_responses(model: &str, tools: bool) -> bool {
    if RESPONSES_ONLY.iter().any(|p| model.contains(p)) {
        return true;
    }
    if !tools {
        return false;
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
    fn a_session_goes_to_the_live_api_only_with_a_live_model() {
        let ask = |kind| Ask {
            kind,
            format: "gemini-live",
            model: None,
            client: None,
            tools: false,
        };
        let sent = |kind, id: &str| format_for(&ask(kind), &Model::new(id));
        assert_eq!(sent("realtime", "gpt-live-1"), Some(OpenAiLive::NAME));
        let transcriber = sent("realtime", "gpt-live-transcribe");
        assert_eq!(transcriber, Some(OpenAiRealtime::NAME));
        assert_eq!(sent("chat", "gpt-live-1"), None);
    }

    #[test]
    fn a_chat_goes_to_responses_for_a_model_only_it_serves_or_tools_it_refuses() {
        for model in [
            "gpt-5.5-pro",
            "gpt-5-codex",
            "o3-deep-research",
            "computer-use-preview",
        ] {
            assert!(in_responses(model, false), "{model}");
        }
        for model in ["gpt-5.4", "gpt-5.6-terra", "gpt-6-luna"] {
            assert!(in_responses(model, true), "{model}");
            assert!(!in_responses(model, false), "{model} without tools");
        }
        for model in ["gpt-5.2", "gpt-5-mini", "gpt-4o", "o4-mini"] {
            assert!(!in_responses(model, true), "{model}");
        }
    }

    #[test]
    fn its_format_follows_the_request() {
        let ask = |format, tools| Ask {
            kind: "chat",
            format,
            model: None,
            client: None,
            tools,
        };
        let model = Model::new("gpt-5.6-terra");
        let format = |a: &Ask| format_for(a, &model);
        assert_eq!(format(&ask("openai-chat", false)), None);
        assert_eq!(
            format(&ask("openai-chat", true)),
            Some(OpenAiResponses::NAME)
        );
        assert_eq!(
            format(&ask("anthropic-messages", true)),
            Some(OpenAiResponses::NAME)
        );
    }
}
