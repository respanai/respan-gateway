//! A provider's models: as OpenAI lists them (`GET /models`), unless its
//! vendor lists them its own way ([`Vendor::models`](crate::Vendor::models)).

use std::future::Future;
use std::pin::Pin;

use chrono::DateTime;
use respan_gateway_core::Fault;
use respan_gateway_http::{Body, http};
use respan_gateway_http_faults::built_wrong;
use respan_gateway_http_transport::json;
pub use respan_gateway_sdk::{Capability, Modality, Model, Price, time};
use serde_json::Value;

use crate::VendorProvider;

/// How a vendor lists a provider's models: with its client
/// ([`VendorProvider::client`]), as it answers.
pub type ListModels = for<'a> fn(&'a VendorProvider) -> BoxFuture<'a, Models>;

/// A future to come, boxed.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The models a provider lists.
pub type Models = Result<Vec<Model>, Fault>;

/// The models of an answer as OpenAI's, `{"data": [{"id", "created"}]}`,
/// or a bare array as Together's, with what OpenAI-compatible lists add read where they say it: a
/// name, a description, a context length, an output limit, modalities
/// and pricing (OpenRouter's per token, Together's per million).
pub fn listed(answer: &Value) -> Vec<Model> {
    let models = answer.as_array().or_else(|| answer["data"].as_array());
    models
        .into_iter()
        .flatten()
        .filter_map(compatible)
        .collect()
}

/// The kind each of Mistral's capability flags says a model is, the first
/// it has the one: a model that chats and hears audio chats.
const KINDS: &[(&str, &str)] = &[
    ("completion_chat", "chat"),
    ("audio_transcription_realtime", "realtime"),
    ("audio_transcription", "transcription"),
    ("audio_speech", "speech"),
    ("ocr", "ocr"),
    ("moderation", "moderation"),
];

/// One entry of an OpenAI-compatible list.
fn compatible(m: &Value) -> Option<Model> {
    let text = |keys: &[&str]| keys.iter().find_map(|k| m[k].as_str().map(str::to_owned));
    let count = |keys: &[&str]| keys.iter().find_map(|k| m[k].as_u64());
    let mut model = Model::new(m["id"].as_str()?);
    model.created = m["created"]
        .as_i64()
        .and_then(|s| DateTime::from_timestamp(s, 0));
    model.display_name = text(&["display_name", "name"]).filter(|n| *n != model.id);
    model.description = text(&["description"]).filter(|d| !d.is_empty());
    model.context_window = count(&["context_length", "context_window", "max_context_length"]);
    model.max_output_tokens = m["top_provider"]["max_completion_tokens"]
        .as_u64()
        .or_else(|| count(&["max_completion_tokens", "max_output_tokens"]));
    names(&m["architecture"]["input_modalities"])
        .filter_map(Modality::parse)
        .for_each(|kind| model.reads(kind));
    names(&m["supported_parameters"])
        .filter_map(Capability::parse)
        .for_each(|can| model.can(can));
    // Mistral's: each a flag, which says its kind too.
    let flagged = |flag: &&(&str, &str)| m["capabilities"][flag.0] == true;
    model.kind = KINDS
        .iter()
        .find(flagged)
        .map(|(_, kind)| (*kind).to_owned());
    let said = m["capabilities"].as_object().into_iter().flatten();
    for (name, _) in said.filter(|(_, on)| **on == true) {
        if let Some(kind) = Modality::parse(name) {
            model.reads(kind);
        }
        if let Some(can) = Capability::parse(name) {
            model.can(can);
        }
    }
    model.price = pricing(&m["pricing"]);
    model.retires_at = text(&["shutdown_date", "deprecation_date"]).and_then(|t| time(&t));
    Some(model)
}

/// The names a list holds at `v`.
fn names(v: &Value) -> impl Iterator<Item = &str> {
    v.as_array().into_iter().flatten().filter_map(Value::as_str)
}

/// A list's pricing: OpenRouter's, `prompt` and `completion` in dollars per
/// token as text; else Together's, `input` and `output` per million. A
/// negative price, as OpenRouter's placeholder `"-1"`, is unknown.
fn pricing(p: &Value) -> Option<Price> {
    let number = |v: &Value| {
        let n: f64 = v.as_f64().or_else(|| v.as_str()?.parse().ok())?;
        (n >= 0.0).then_some(n)
    };
    if let (Some(input), Some(output)) = (number(&p["prompt"]), number(&p["completion"])) {
        let per_million = |key| number(&p[key]).map(|n| n * 1e6);
        let mut price = Price::new(input * 1e6, output * 1e6, per_million("input_cache_read"));
        price.cache_write = per_million("input_cache_write");
        return Some(price);
    }
    let (input, output) = (number(&p["input"])?, number(&p["output"])?);
    Some(Price::new(input, output, number(&p["cached_input"])))
}

/// OpenAI's model list: `GET /models`.
pub fn openai(p: &VendorProvider) -> BoxFuture<'_, Models> {
    Box::pin(async move {
        let call = http::Request::get("/models").body(Body::default());
        let (answer, _) = p.client()?.send(call.map_err(built_wrong)?).await?;
        Ok(listed(&json(answer).await?))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn what_compatible_lists_add_is_read_where_they_say_it() {
        let answer = json!({ "data": [
            // OpenRouter's: per-token prices as text, modalities, parameters.
            { "id": "anthropic/claude-x", "name": "Anthropic: Claude X", "context_length": 200000,
              "architecture": { "input_modalities": ["text", "image"] },
              "top_provider": { "max_completion_tokens": 64000 },
              "pricing": { "prompt": "0.000003", "completion": "0.000015", "input_cache_read": "0.0000003",
                           "input_cache_write": "0.00000375" },
              "supported_parameters": ["tools", "reasoning", "temperature"] },
            // Together's: per-million prices, a display name.
            { "id": "meta-llama/Llama-3", "display_name": "Llama 3", "context_length": 8192,
              "pricing": { "input": 0.88, "output": 0.88 } },
            // Mistral's: capabilities, a context length of its own name.
            { "id": "mistral-small", "max_context_length": 32000, "description": "Small",
              "capabilities": { "completion_chat": true, "vision": true, "function_calling": true } },
            // OpenAI's own: an id and when, and a retirement.
            { "id": "gpt-old", "created": 1, "shutdown_date": "2026-10-23" },
        ] });
        let models = listed(&answer);
        let router = &models[0];
        assert_eq!(router.display_name.as_deref(), Some("Anthropic: Claude X"));
        assert_eq!(
            (router.context_window, router.max_output_tokens),
            (Some(200000), Some(64000))
        );
        assert_eq!(router.input_modalities, [Modality::Image]);
        assert_eq!(
            router.capabilities,
            [Capability::Tools, Capability::Reasoning]
        );
        let price = router.price.expect("priced");
        assert!((price.input - 3.0).abs() < 1e-9 && (price.output - 15.0).abs() < 1e-9);
        assert!((price.cached_input.unwrap_or_default() - 0.3).abs() < 1e-9);
        assert!((price.cache_write.unwrap_or_default() - 3.75).abs() < 1e-9);
        assert_eq!(models[1].price, Some(Price::new(0.88, 0.88, None)));
        assert_eq!(models[2].context_window, Some(32000));
        assert_eq!(models[2].input_modalities, [Modality::Image]);
        assert_eq!(models[2].capabilities, [Capability::Tools]);
        assert_eq!(models[2].kind.as_deref(), Some("chat"));
        assert_eq!(models[0].kind, None, "a list that says no kind");
        let retires = models[3].retires_at.map(|t| t.timestamp());
        assert_eq!(retires, Some(1_792_713_600));
        assert_eq!(models[3].created.map(|t| t.timestamp()), Some(1));
        assert_eq!(models[3].price, None);
    }

    #[test]
    fn mistrals_flags_say_its_models_kinds() {
        let answer = json!({ "data": [
            { "id": "mistral-ocr-4", "capabilities": { "ocr": true, "vision": true, "function_calling": true } },
            { "id": "voxtral-small", "capabilities": { "audio": true, "completion_chat": true } },
            { "id": "voxtral-mini-transcribe-realtime-2602", "capabilities": { "audio_transcription_realtime": true } },
            { "id": "voxtral-mini-transcribe-3", "capabilities": { "audio_transcription": true } },
            { "id": "voxtral-mini-tts-3", "capabilities": { "audio_speech": true } },
            { "id": "mistral-moderation", "capabilities": { "classification": true, "moderation": true } },
            { "id": "mistral-embed", "capabilities": { "completion_chat": false } },
        ] });
        let kinds: Vec<_> = listed(&answer).into_iter().map(|m| m.kind).collect();
        let said = |k: &str| Some(k.to_owned());
        assert_eq!(
            kinds,
            [
                said("ocr"),
                said("chat"),
                said("realtime"),
                said("transcription"),
                said("speech"),
                said("moderation"),
                None,
            ]
        );
    }

    #[test]
    fn a_placeholder_price_is_unknown_and_a_bare_array_a_list() {
        let answer = json!([
            { "id": "auto", "pricing": { "prompt": "-1", "completion": "-1" } },
            { "id": "free", "pricing": { "prompt": "0", "completion": "0", "input_cache_read": "-1" } },
        ]);
        let models = listed(&answer);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].price, None);
        assert_eq!(models[1].price, Some(Price::new(0.0, 0.0, None)));
    }

    #[test]
    fn a_time_or_a_date_is_read_in_utc() {
        let at = |text| time(text).map(|t| t.timestamp());
        assert_eq!(at("1970-01-01"), Some(0));
        assert_eq!(at("2026-10-23"), Some(1_792_713_600));
        assert_eq!(at("2026-10-23T00:00:00Z"), Some(1_792_713_600));
        assert_eq!(at("2026-10-23T01:30:00.5+01:30"), Some(1_792_713_600));
        assert_eq!(at("soon"), None);
        assert_eq!(at("2026-13-01"), None);
    }

    #[test]
    fn every_vendors_name_of_a_modality_or_capability_is_one() {
        for name in ["file", "pdf", "document"] {
            assert_eq!(Modality::parse(name), Some(Modality::Document));
        }
        assert_eq!(Modality::parse("text"), None);
        assert_eq!(Capability::parse("thinking"), Some(Capability::Reasoning));
        assert_eq!(
            Capability::parse("function_calling"),
            Some(Capability::Tools)
        );
        let mut model = Model::new("m");
        model.reads(Modality::Image);
        model.reads(Modality::Image);
        assert_eq!(model.input_modalities, [Modality::Image]);
    }
}
