//! DeepInfra: Chat Completions, images, embeddings, speech and
//! transcriptions, as OpenAI serves them, and rerank on its own inference
//! API, in its dialect of Cohere's. Not its videos: a finished job says
//! `succeeded`, not `completed`, beside a `data` list of its files.

use respan_gateway_conversation::Usage;
use respan_gateway_conversation_http::fields::{kept, native_of, rest_of};
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::Fault;
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::path::set_path;
use respan_gateway_http::{Decoder, Whole};
use respan_gateway_media::rerank;
use respan_gateway_media_http::cohere::CohereRerank;
use respan_gateway_media_http::dialect::{Dialect, Dialected, OwnCall};
use respan_gateway_media_http::openai::{
    OpenAiEmbeddings, OpenAiImages, OpenAiSpeech, OpenAiTranscriptions,
};

use serde_json::Value;

use respan_gateway_vendor_kit::{
    Body, BoxFuture, Model, Models, Price, Vendor, VendorProvider, built_wrong, http, json, time,
};

/// Where its API is.
const BASE_URL: &str = "https://api.deepinfra.com/v1/openai";

/// Where its own inference API is, a model's at `/{model}`.
const INFERENCE_URL: &str = "https://api.deepinfra.com/v1/inference";

/// DeepInfra.
pub const VENDOR: Vendor = Vendor {
    id: "deepinfra",
    key_env: &["DEEPINFRA_API_KEY"],
    formats: &[
        (&OpenAiChat, Some(BASE_URL)),
        (&OpenAiImages, Some(BASE_URL)),
        (&OpenAiEmbeddings, Some(BASE_URL)),
        (&OpenAiSpeech, Some(BASE_URL)),
        (&OpenAiTranscriptions, Some(BASE_URL)),
        (
            &Dialected::new(CohereRerank, DeepInfra),
            Some(INFERENCE_URL),
        ),
    ],
    models: list_models,
    ..Vendor::DEFAULT
};

/// DeepInfra's rerank: a `POST` to its inference API at the model's path,
/// `/{model}`, of `queries` (the one query) and `documents`, answered with
/// `scores` in the documents' order and `input_tokens`. It takes no
/// `top_n` or `return_documents`: its answer is ranked best first, cut to
/// `top_n` and given its documents here, as Cohere's would be. Its
/// `inference_status`, with what the call cost, is a native field.
#[derive(Debug, Clone, Copy)]
pub struct DeepInfra;

impl Dialect<rerank::Input, rerank::Output> for DeepInfra {
    fn call(
        &self,
        head: &mut RequestHead,
        input: &rerank::Input,
    ) -> Option<Result<OwnCall<rerank::Input, rerank::Output>, Fault>> {
        Some(rerank_call(head, input))
    }
}

/// A rerank call of `input`, at its model's path.
fn rerank_call(
    head: &mut RequestHead,
    input: &rerank::Input,
) -> Result<OwnCall<rerank::Input, rerank::Output>, Fault> {
    let model = input
        .model
        .as_deref()
        .ok_or_else(|| Fault::bad_request("deepinfra rerank needs a 'model'"))?;
    if !is_model_path(model) {
        return Err(Fault::bad_request(format!(
            "'{model}' is no deepinfra model"
        )));
    }
    head.method = http::Method::POST;
    set_path(head, &format!("/{model}"))?;
    let json = http::HeaderValue::from_static("application/json");
    head.headers.insert(http::header::CONTENT_TYPE, json);
    let mut v = kept(CohereRerank::NAME, &input.native)?;
    v.insert("queries".into(), vec![input.query.clone()].into());
    v.insert("documents".into(), input.documents.clone().into());
    let body = Bytes::from(
        serde_json::to_vec(&v)
            .map_err(|e| Fault::internal("a body could not be written").with_source(e))?,
    );
    let asked = input.clone();
    let answer = move |raw: Bytes, out: &mut Vec<rerank::Output>| ranked(&asked, &raw, out);
    Ok(OwnCall {
        body: Box::new(move |_: &rerank::Input, out: &mut Vec<Bytes>| {
            out.push(body.clone());
            Ok(())
        }),
        answer: Box::new(Whole::new(answer)) as Box<dyn Decoder<rerank::Output>>,
    })
}

/// Whether `model` is a path of its names, `owner/name`, of letters,
/// digits, `-`, `_` and `.`, which a client cannot steer elsewhere.
fn is_model_path(model: &str) -> bool {
    let named = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
    let segment = |s: &str| !s.is_empty() && s != "." && s != ".." && s.chars().all(named);
    model.split('/').all(segment)
}

/// The events of its answer `raw` to `asked`: the documents by score, best
/// first, as many as `top_n` asks, their text if `return_documents` does.
fn ranked(asked: &rerank::Input, raw: &[u8], out: &mut Vec<rerank::Output>) -> Result<(), Fault> {
    let v: Value = serde_json::from_slice(raw)
        .map_err(|e| Fault::upstream("deepinfra's rerank answer is no JSON").with_source(e))?;
    let scores = v["scores"]
        .as_array()
        .ok_or_else(|| Fault::upstream("deepinfra's rerank answer has no scores"))?;
    let mut start = rerank::Start::new(asked.model.clone());
    start.id = v["request_id"].as_str().map(str::to_owned);
    let rest = rest_of(&v, &["scores", "input_tokens", "request_id"]);
    start.native = native_of(CohereRerank::NAME, rest);
    out.push(rerank::Output::Start(start));
    let mut results = Vec::with_capacity(scores.len());
    for (index, score) in scores.iter().enumerate() {
        let score = score
            .as_f64()
            .ok_or_else(|| Fault::upstream("deepinfra's rerank score is no number"))?;
        results.push((index, score));
    }
    results.sort_by(|a, b| b.1.total_cmp(&a.1));
    let shown = asked.top_n.map_or(results.len(), |n| n as usize);
    for (index, score) in results.into_iter().take(shown) {
        let mut ranked = rerank::Ranked::new(index as u32, score);
        if asked.return_documents == Some(true) {
            ranked.document = asked.documents.get(index).cloned();
        }
        out.push(rerank::Output::Result(ranked));
    }
    if let Some(tokens) = v["input_tokens"].as_u64() {
        out.push(rerank::Output::Usage(Usage::new(tokens, 0)));
    }
    Ok(())
}

/// The kind each of its tags says a model is, the first it has.
const KINDS: &[(&str, &str)] = &[
    ("chat", "chat"),
    ("embed", "embedding"),
    ("image-gen", "image"),
    ("video-gen", "video"),
    ("tts", "speech"),
    ("stt", "transcription"),
];

/// Its models: `GET /models` as OpenAI lists them, each with what it
/// does, its limits and its price in its `metadata`.
fn list_models(p: &VendorProvider) -> BoxFuture<'_, Models> {
    Box::pin(async move {
        let call = http::Request::get("/models").body(Body::default());
        let (answer, _) = p.client()?.send(call.map_err(built_wrong)?).await?;
        let mut models = listed(&json(answer).await?);
        // Its rerankers are only in its own list, beside its inference API.
        let inference = p.base_url_of(CohereRerank::NAME).unwrap_or(INFERENCE_URL);
        let root = inference
            .trim_end_matches('/')
            .trim_end_matches("/v1/inference");
        let call = http::Request::get("/models/list").body(Body::default());
        let (answer, _) = p.client_at(root)?.send(call.map_err(built_wrong)?).await?;
        models.extend(rerankers(&json(answer).await?));
        Ok(models)
    })
}

/// The rerankers of its own list, `GET /models/list`: each its
/// `model_name`, a `type` of `reranker`, its context and its price in cents
/// per input token.
fn rerankers(answer: &Value) -> Vec<Model> {
    let entries = answer.as_array().into_iter().flatten();
    let reranker = |m: &&Value| m["type"] == "reranker";
    entries.filter(reranker).filter_map(reranker_of).collect()
}

/// One reranker of its own list.
fn reranker_of(m: &Value) -> Option<Model> {
    let mut model = Model::new(m["model_name"].as_str()?);
    model.kind = Some("rerank".to_owned());
    model.description = m["description"]
        .as_str()
        .filter(|d| !d.is_empty())
        .map(str::to_owned);
    model.context_window = m["max_tokens"].as_u64();
    model.created = m["create_ts"].as_str().and_then(time);
    let cents = m["pricing"]["cents_per_input_token"].as_f64();
    model.price = cents.map(|c| Price::new(c * 1e4, 0.0, None));
    Some(model)
}

/// The models of its list, each of the kind its tags say; one whose tags
/// say none of [`KINDS`] is none it serves, and is left out.
fn listed(answer: &Value) -> Vec<Model> {
    let entries = answer["data"].as_array().into_iter().flatten();
    entries.filter_map(described).collect()
}

/// One entry of its list: its id, and the rest from its `metadata`. Its
/// `created` is always 0, so says nothing.
fn described(m: &Value) -> Option<Model> {
    let mut model = Model::new(m["id"].as_str()?);
    let about = &m["metadata"];
    let tags = about["tags"].as_array();
    let has = |tag: &str| tags.is_some_and(|t| t.iter().any(|t| t == tag));
    let (_, kind) = KINDS.iter().find(|(tag, _)| has(tag))?;
    model.kind = Some((*kind).to_owned());
    model.description = about["description"]
        .as_str()
        .filter(|d| !d.is_empty())
        .map(str::to_owned);
    model.context_window = about["context_length"].as_u64();
    model.max_output_tokens = about["max_tokens"].as_u64();
    model.price = price(&about["pricing"]);
    Some(model)
}

/// A price by the token, in dollars per million: an embedding model's has
/// no output. Its prices by the image, character or second are not one.
fn price(p: &Value) -> Option<Price> {
    let input = p["input_tokens"].as_f64()?;
    let output = p["output_tokens"].as_f64().unwrap_or_default();
    Some(Price::new(input, output, p["cache_read_tokens"].as_f64()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn its_tags_say_a_models_kind_and_its_metadata_the_rest() {
        let entry = |id: &str, tags: Value, pricing: Value| {
            let about = json!({ "description": "", "context_length": 8192, "max_tokens": 4096,
                                "pricing": pricing, "tags": tags });
            json!({ "id": id, "object": "model", "created": 0, "metadata": about })
        };
        let answer = json!({ "object": "list", "data": [
            entry("Qwen/Qwen3-30B-A3B", json!(["chat", "reasoning"]),
                  json!({ "input_tokens": 0.12, "output_tokens": 0.5, "cache_read_tokens": 0.06 })),
            entry("BAAI/bge-m3", json!(["embed"]), json!({ "input_tokens": 0.01 })),
            entry("black-forest-labs/FLUX-1-dev", json!(["image-gen"]), json!({ "per_image_unit": 0.04 })),
            entry("google/veo-3.1", json!(["video-gen"]), json!({ "output_seconds": 0.4 })),
            entry("hexgrad/Kokoro-82M", json!(["tts"]), json!({ "input_characters": 0.8 })),
            entry("openai/whisper-large-v3", json!(["stt"]), json!({ "input_seconds": 0.00001 })),
            entry("BAAI/bge-reranker-v2-m3", json!(["reranker"]), json!({ "input_tokens": 0.01 })),
        ] });
        let models = listed(&answer);
        let kinds: Vec<_> = models.iter().map(|m| m.kind.as_deref()).collect();
        assert_eq!(
            kinds,
            [
                Some("chat"),
                Some("embedding"),
                Some("image"),
                Some("video"),
                Some("speech"),
                Some("transcription"),
            ],
            "its rerankers are read from its own list"
        );
        let chat = &models[0];
        assert_eq!(
            (chat.context_window, chat.max_output_tokens),
            (Some(8192), Some(4096))
        );
        assert_eq!(chat.description, None);
        assert_eq!(chat.price, Some(Price::new(0.12, 0.5, Some(0.06))));
        assert_eq!(models[1].price, Some(Price::new(0.01, 0.0, None)));
        assert_eq!(models[2].price, None);
    }

    #[test]
    fn its_rerankers_come_from_its_own_list() {
        let answer = json!([
            { "model_name": "Qwen/Qwen3-Reranker-0.6B", "type": "reranker", "description": "Qwen3",
              "max_tokens": 32768, "create_ts": "2025-07-01T21:00:35+00:00",
              "pricing": { "type": "input_tokens", "cents_per_input_token": 1e-6 } },
            { "model_name": "BAAI/bge-m3", "type": "embeddings" },
        ]);
        let models = rerankers(&answer);
        assert_eq!(models.len(), 1);
        let qwen = &models[0];
        assert_eq!(qwen.kind.as_deref(), Some("rerank"));
        assert_eq!(qwen.context_window, Some(32768));
        assert!(qwen.created.is_some());
        let price = qwen.price.expect("priced");
        assert!((price.input - 0.01).abs() < 1e-12 && price.output == 0.0);
    }

    #[test]
    fn a_rerank_is_called_at_its_models_path_and_answered_ranked() {
        let mut asked =
            rerank::Input::new("q", vec!["a".to_owned(), "b".to_owned(), "c".to_owned()]);
        asked.model = Some("Qwen/Qwen3-Reranker-0.6B".to_owned());
        asked.top_n = Some(2);
        asked.return_documents = Some(true);
        let (mut head, _) = http::Request::new(()).into_parts();
        let call = rerank_call(&mut head, &asked).expect("a call");
        assert_eq!(head.uri.path(), "/Qwen/Qwen3-Reranker-0.6B");
        let mut sent = Vec::new();
        let mut body = call.body;
        body.encode(&asked, &mut sent).expect("a body");
        let sent: Value = serde_json::from_slice(&sent[0]).unwrap_or_default();
        assert_eq!(
            sent,
            json!({ "queries": ["q"], "documents": ["a", "b", "c"] })
        );
        let answer = json!({ "scores": [0.1, 0.9, 0.5], "input_tokens": 12, "request_id": "r",
                             "inference_status": { "cost": 1.2e-6 } });
        let mut out = Vec::new();
        ranked(&asked, answer.to_string().as_bytes(), &mut out).expect("events");
        let results: Vec<_> = out
            .iter()
            .filter_map(|e| match e {
                rerank::Output::Result(r) => Some((r.index, r.document.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            results,
            [(1, Some("b".to_owned())), (2, Some("c".to_owned()))]
        );
        assert!(out.contains(&rerank::Output::Usage(Usage::new(12, 0))));
    }

    #[test]
    fn a_model_that_would_leave_its_path_is_refused() {
        assert!(is_model_path("BAAI/bge-reranker-v2-m3"));
        for bad in ["../openai/models", "a/../b", "a?x=1", "a b", "", "a//b"] {
            assert!(!is_model_path(bad), "{bad}");
        }
    }
}
