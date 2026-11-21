//! Cohere: rerank on its own API, and Chat Completions and embeddings on
//! its OpenAI compatibility API. Not its transcription, which takes its
//! form's fields only before the file, and a `language` always; nor its
//! image embedding models, its `parse` or its native chat.

use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_media_http::cohere::CohereRerank;
use respan_gateway_media_http::openai::OpenAiEmbeddings;
use serde_json::Value;

use respan_gateway_vendor_kit::{
    Body, BoxFuture, Capability, Modality, Model, Models, Vendor, VendorProvider, built_wrong,
    http, json,
};

/// Where its own API is, with the version its rerank takes.
const BASE_URL: &str = "https://api.cohere.com/v2";

/// Where its OpenAI compatibility API is.
const COMPATIBLE_URL: &str = "https://api.cohere.ai/compatibility/v1";

/// Where its model list is.
const MODELS_URL: &str = "https://api.cohere.com/v1";

/// Cohere. Its SDKs take the key from `CO_API_KEY`.
pub const VENDOR: Vendor = Vendor {
    id: "cohere",
    key_env: &["COHERE_API_KEY", "CO_API_KEY"],
    formats: &[
        (&CohereRerank, Some(BASE_URL)),
        (&OpenAiChat, Some(COMPATIBLE_URL)),
        (&OpenAiEmbeddings, Some(COMPATIBLE_URL)),
    ],
    models: list_models,
    ..Vendor::DEFAULT
};

/// The kind each of its endpoints says a model is, the first it has:
/// `generate` and `summarize` are older chats'.
const KINDS: &[(&str, &str)] = &[
    ("chat", "chat"),
    ("embed", "embedding"),
    ("rerank", "rerank"),
];

/// Its models: `GET /v1/models` of its own API, which says each one's
/// endpoints, context length and features; the compatibility API's list
/// says only ids.
fn list_models(p: &VendorProvider) -> BoxFuture<'_, Models> {
    Box::pin(async move {
        let call = http::Request::get("/models?page_size=1000").body(Body::default());
        let (answer, _) = p
            .client_at(MODELS_URL)?
            .send(call.map_err(built_wrong)?)
            .await?;
        Ok(listed(&json(answer).await?))
    })
}

/// The models of its list, each of the kind its endpoints say; one whose
/// endpoints say none of [`KINDS`] is none it serves, and is left out.
fn listed(answer: &Value) -> Vec<Model> {
    let entries = answer["models"].as_array().into_iter().flatten();
    entries.filter_map(described).collect()
}

/// One entry of its list.
fn described(m: &Value) -> Option<Model> {
    let endpoints = m["endpoints"].as_array()?;
    let has = |name: &str| endpoints.iter().any(|e| e == name);
    let (_, kind) = KINDS.iter().find(|(endpoint, _)| has(endpoint))?;
    let mut model = Model::new(m["name"].as_str()?);
    model.kind = Some((*kind).to_owned());
    model.context_window = m["context_length"].as_u64().filter(|n| *n > 0);
    let features = m["features"].as_array().into_iter().flatten();
    for feature in features.filter_map(Value::as_str) {
        if let Some(kind) = Modality::parse(feature) {
            model.reads(kind);
        }
        if let Some(can) = Capability::parse(feature) {
            model.can(can);
        }
    }
    Some(model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn its_endpoints_say_a_models_kind() {
        let entry = |name: &str, endpoints: Value, features: Value| json!({ "name": name, "endpoints": endpoints, "context_length": 4096, "features": features });
        let answer = json!({ "models": [
            entry("command-a-03-2025", json!(["chat"]), json!(["tools", "json_mode"])),
            entry("command-r-08-2024", json!(["generate", "chat", "summarize"]), Value::Null),
            entry("embed-v4.0", json!(["embed"]), Value::Null),
            entry("rerank-v3.5", json!(["rerank"]), Value::Null),
            entry("embed-english-v3.0-image", json!(["embed_image"]), Value::Null),
            entry("cohere-transcribe-03-2026", json!(["transcriptions"]), Value::Null),
            entry("parse-v5.0", json!(["parse"]), json!(["vision"])),
        ] });
        let models = listed(&answer);
        let kinds: Vec<_> = models
            .iter()
            .map(|m| (m.id.as_str(), m.kind.as_deref()))
            .collect();
        assert_eq!(
            kinds,
            [
                ("command-a-03-2025", Some("chat")),
                ("command-r-08-2024", Some("chat")),
                ("embed-v4.0", Some("embedding")),
                ("rerank-v3.5", Some("rerank")),
            ]
        );
        assert_eq!(models[0].capabilities, [Capability::Tools]);
        assert_eq!(models[3].context_window, Some(4096));
    }
}
