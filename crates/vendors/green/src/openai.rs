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

use respan_gateway_vendor_kit::chat::{OpenAi, in_responses};
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
