//! OpenAI: Chat Completions, Responses, Realtime and Responses over WebSocket, its
//! media endpoints (images, transcriptions, speech, embeddings), and its
//! decisions. Its Videos API and Sora models were removed on 2026-09-24, with
//! no replacement; the format stays, as what clients send for any provider of
//! video.

use respan_gateway_conversation::{Input, Output};
use respan_gateway_conversation_http::Schema;
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_conversation_http::responses::OpenAiResponses;
use respan_gateway_core::Fault;
use respan_gateway_decision_http::OpenAiDecisions;
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_media_http::openai::{
    OpenAiEmbeddings, OpenAiImages, OpenAiSpeech, OpenAiTranscriptions,
};
use respan_gateway_realtime_ws::{OpenAiRealtime, ResponsesWs};
use serde_json::{Map, Value};

use respan_gateway_vendor_kit::{Ask, Dialect, Dialected, Model, Rewrite, Vendor};

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
        (&OpenAiDecisions::DEFAULT, Some(BASE_URL)),
    ],
    format_for: Some(format_for),
    ..Vendor::DEFAULT
};

/// OpenAI's Chat Completions: its limit is `max_completion_tokens`, as its
/// reasoning models take no `max_tokens`, whatever the client spelled.
#[derive(Debug, Clone, Copy)]
pub struct OpenAi;

impl Dialect for OpenAi {
    type Input = Input;
    type Output = Output;

    fn request(&self, _: &mut RequestHead) -> Result<Option<Rewrite>, Fault> {
        Ok(Some(Box::new(completion_limit)))
    }
}

/// A call's JSON with its `max_tokens` as `max_completion_tokens`.
fn completion_limit(body: Bytes) -> Result<Bytes, Fault> {
    if !body.windows(12).any(|w| w == b"\"max_tokens\"") {
        return Ok(body);
    }
    let unwritten = |e| Fault::internal("a call is no JSON object").with_source(e);
    let mut call: Map<String, Value> = serde_json::from_slice(&body).map_err(unwritten)?;
    let Some(limit) = call.shift_remove("max_tokens") else {
        return Ok(body);
    };
    call.entry("max_completion_tokens").or_insert(limit);
    serde_json::to_vec(&call)
        .map(Bytes::from)
        .map_err(unwritten)
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
    fn its_chat_limit_is_max_completion_tokens() {
        let sent = |body: &str| {
            let out = completion_limit(Bytes::from(body.to_owned())).expect("written");
            serde_json::from_slice::<Value>(&out).expect("json")
        };
        let renamed = sent(r#"{"model":"gpt-5-mini","max_tokens":7,"messages":[]}"#);
        assert_eq!(renamed["max_completion_tokens"], 7);
        assert!(renamed.get("max_tokens").is_none());
        let both = sent(r#"{"max_tokens":7,"max_completion_tokens":7}"#);
        assert_eq!(both, serde_json::json!({ "max_completion_tokens": 7 }));
        let body = Bytes::from_static(br#"{"max_completion_tokens":7}"#);
        assert_eq!(completion_limit(body.clone()).ok(), Some(body));
    }
}
