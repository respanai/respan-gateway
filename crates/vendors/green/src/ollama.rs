//! Ollama: Chat Completions and embeddings, as OpenAI serves them, from a
//! local server. Verified end to end on Ollama 0.40.11 with chat (streaming
//! and not), tool calls, embeddings (float and base64) and model listing,
//! from both the OpenAI and the Anthropic client shapes.

use respan_gateway_conversation::{Input, Output};
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::Filter;
use respan_gateway_media_http::openai::OpenAiEmbeddings;

use respan_gateway_vendor_kit::{
    Body, BoxFuture, Dialect, Dialected, Model, Models, Upstream, Vendor, VendorProvider,
    built_wrong, chat, http, json,
};

/// Where its API is.
const BASE_URL: &str = "http://localhost:11434/v1";

/// Ollama, on this machine. It needs no key; `OLLAMA_HOST` says where it is.
pub const VENDOR: Vendor = Vendor {
    id: "ollama",
    key_env: &[],
    formats: &[
        (&Dialected::new(OpenAiChat, OllamaChat), Some(BASE_URL)),
        (&OpenAiEmbeddings, Some(BASE_URL)),
    ],
    models: list_models,
    ..Vendor::DEFAULT
};

/// Ollama's Chat Completions: it takes a `top_k` the spec has no field for.
#[derive(Debug, Clone, Copy)]
pub struct OllamaChat;

impl Dialect for OllamaChat {
    type Input = Input;
    type Output = Output;

    fn inputs(&self) -> Option<Box<dyn Filter<Input>>> {
        Some(Box::new(chat::write_top_k))
    }
}

/// Its models, of its native `GET /api/tags`: each the kind its
/// `capabilities` say, `embedding` or chat. Ollama's OpenAI-style
/// `GET /models` says no kind of any model, so a lister of it alone would
/// name an embedding model a chat one. When the tags are out of reach, as
/// on a server old enough to serve no `/api/tags`, it falls back to that
/// OpenAI list, every model a chat one.
fn list_models(p: &VendorProvider) -> BoxFuture<'_, Models> {
    Box::pin(async move {
        // The tags live beside `/v1`, at the server's origin.
        let base = p.base_url_of(OpenAiChat.name()).unwrap_or(BASE_URL);
        let origin = base.strip_suffix("/v1").unwrap_or(base);
        if let Ok(client) = p.client_at(origin) {
            let call = http::Request::get("/api/tags").body(Body::default());
            if let Ok((answer, _)) = client.send(call.map_err(built_wrong)?).await {
                return Ok(listed(&json(answer).await?));
            }
        }
        let call = http::Request::get("/models").body(Body::default());
        let (answer, _) = p.client()?.send(call.map_err(built_wrong)?).await?;
        Ok(listed_as_openai(&json(answer).await?))
    })
}

/// One entry of the tags: the kind its `capabilities` say, `embedding` or
/// chat. (The model's `family` of `bert` says the same of the embedding
/// ones, but the capability is the data.) A model the tags say no
/// capability of, one an older server lists, is a chat one.
fn described(m: &Value) -> Option<Model> {
    let mut model = Model::new(m["name"].as_str()?);
    let embedding = m["capabilities"]
        .as_array()
        .is_some_and(|cs| cs.iter().any(|c| c.as_str() == Some("embedding")));
    model.kind = Some(if embedding { "embedding" } else { "chat" }.to_owned());
    Some(model)
}

/// The models of its tags.
fn listed(answer: &Value) -> Vec<Model> {
    let entries = answer["models"].as_array().into_iter().flatten();
    entries.filter_map(described).collect()
}

/// The models of its OpenAI-style list, every one a chat one, of a server
/// the tags said nothing to.
fn listed_as_openai(answer: &Value) -> Vec<Model> {
    let entries = answer["data"].as_array().into_iter().flatten();
    entries
        .filter_map(|m| {
            let id = m["id"].as_str()?;
            let mut model = Model::new(id);
            model.kind = Some("chat".to_owned());
            Some(model)
        })
        .collect()
}

use serde_json::Value;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn its_tags_say_a_models_kind() {
        // From a live Ollama 0.40.11: an embedding model and a chat one.
        let answer = json!({ "models": [
            {
                "name": "all-minilm:l6-v2", "model": "all-minilm:l6-v2",
                "modified_at": "2026-10-25T10:50:04.970958038+04:00", "size": 45960996,
                "digest": "1b226e2802db",
                "details": { "parent_model": "", "format": "gguf", "family": "bert",
                             "families": ["bert"], "parameter_size": "23M",
                             "quantization_level": "F16", "context_length": 512,
                             "embedding_length": 384, "runner": "ggml" },
                "capabilities": ["embedding"]
            },
            {
                "name": "qwen2.5:0.5b", "model": "qwen2.5:0.5b",
                "modified_at": "2026-10-25T10:49:45.891558038+04:00", "size": 397821319,
                "digest": "a8b0c5157701",
                "details": { "parent_model": "", "format": "gguf", "family": "qwen2",
                             "families": ["qwen2"], "parameter_size": "494.03M",
                             "quantization_level": "Q4_K_M", "context_length": 32768,
                             "embedding_length": 896, "runner": "ggml" },
                "capabilities": ["completion", "tools"]
            },
        ] });
        let models = listed(&answer);
        let kinds: Vec<_> = models.iter().map(|m| m.kind.as_deref()).collect();
        assert_eq!(kinds, [Some("embedding"), Some("chat")]);
    }

    #[test]
    fn a_model_of_no_capabilities_is_a_chat_one() {
        let answer = json!({ "models": [
            { "name": "m", "capabilities": [] },
            { "name": "old" },
        ] });
        let models = listed(&answer);
        let kinds: Vec<_> = models.iter().map(|m| m.kind.as_deref()).collect();
        assert_eq!(kinds, [Some("chat"), Some("chat")]);
    }

    #[test]
    fn its_openai_list_names_every_model_a_chat_one() {
        let answer = json!({ "object": "list", "data": [
            { "id": "all-minilm:l6-v2", "object": "model", "created": 1792911004, "owned_by": "library" },
            { "id": "qwen2.5:0.5b", "object": "model", "created": 1792910985, "owned_by": "library" },
        ] });
        let models = listed_as_openai(&answer);
        let kinds: Vec<_> = models.iter().map(|m| m.kind.as_deref()).collect();
        assert_eq!(kinds, [Some("chat"), Some("chat")]);
    }
}
