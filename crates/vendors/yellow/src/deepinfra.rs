//! DeepInfra: Chat Completions, images, embeddings, speech and
//! transcriptions, as OpenAI serves them, and videos in its dialect.

use respan_gateway_conversation_http::fields::media_url;
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::{Fault, Filter};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::http::response::Parts as ResponseHead;
use respan_gateway_media::video;
use respan_gateway_media_http::openai::{
    OpenAiEmbeddings, OpenAiImages, OpenAiSpeech, OpenAiTranscriptions, OpenAiVideos,
};

use serde_json::{Map, Value};

use respan_gateway_vendor_kit::{
    Body, BoxFuture, Dialect, Dialected, Model, Models, Price, Rewrite, Vendor, VendorProvider,
    built_wrong, http, json, own_mut,
};

/// Where its API is.
const BASE_URL: &str = "https://api.deepinfra.com/v1/openai";

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
        (&Dialected::new(OpenAiVideos, DeepInfra), Some(BASE_URL)),
    ],
    models: list_models,
    ..Vendor::DEFAULT
};

/// DeepInfra's videos: it takes a creation's reference image as
/// `image_url`, a URL or the image's data URL, in JSON, not as a form's
/// `input_reference`. Its job says `running` and `succeeded` for OpenAI's
/// `in_progress` and `completed`, why it failed as a string, and its files
/// in a `data` list, which would read as a page of jobs; they are dropped,
/// as `/videos/{id}/content` serves the video.
#[derive(Debug, Clone, Copy)]
pub struct DeepInfra;

impl Dialect for DeepInfra {
    type Input = video::Input;
    type Output = video::Output;

    fn inputs(&self) -> Option<Box<dyn Filter<video::Input>>> {
        Some(Box::new(referenced))
    }

    fn response(&self, _: &RequestHead, head: &mut ResponseHead) -> Option<Rewrite> {
        crate::json_answer(head).then(|| Box::new(openai_job) as Rewrite)
    }
}

/// The states of its jobs OpenAI names otherwise, each with OpenAI's name.
const STATUSES: &[(&str, &str)] = &[("running", "in_progress"), ("succeeded", "completed")];

/// A creation as DeepInfra takes it: its reference image as `image_url`.
fn referenced(input: &mut video::Input) -> Result<(), Fault> {
    let video::Input::Create(create) = input else {
        return Ok(());
    };
    let Some(reference) = create.reference.take() else {
        return Ok(());
    };
    let url = media_url(&reference);
    let fields = own_mut(&mut create.native, OpenAiVideos::NAME);
    fields.entry("image_url").or_insert_with(|| url.into());
    Ok(())
}

/// Its job as OpenAI's; an answer that is no JSON object is left as it is.
fn openai_job(raw: Bytes) -> Result<Bytes, Fault> {
    let Ok(mut job) = serde_json::from_slice::<Map<String, Value>>(&raw) else {
        return Ok(raw);
    };
    job.shift_remove("data");
    if let Some(status) = job.get_mut("status")
        && let Some((_, openai)) = STATUSES.iter().find(|(its, _)| status == its)
    {
        *status = (*openai).into();
    }
    if let Some(message) = job.get("error").and_then(Value::as_str) {
        let error = Map::from_iter([("message".to_owned(), message.into())]);
        job.insert("error".into(), error.into());
    }
    serde_json::to_vec(&job)
        .map(Bytes::from)
        .map_err(|e| Fault::internal("a video job is no JSON").with_source(e))
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
        Ok(listed(&json(answer).await?))
    })
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
    use respan_gateway_conversation::{Media, MediaSource};
    use serde_json::json;

    fn head(status: u16, content_type: &str) -> ResponseHead {
        let answer = http::Response::builder()
            .status(status)
            .header("content-type", content_type);
        answer.body(()).expect("head").into_parts().0
    }

    fn job(answer: Value) -> Value {
        let read = openai_job(Bytes::from(answer.to_string())).expect("a job");
        serde_json::from_slice(&read).expect("JSON")
    }

    #[test]
    fn only_a_json_success_is_read_as_a_job() {
        let sent = http::Request::new(()).into_parts().0;
        let rewritten = |status, content_type| {
            let mut head = head(status, content_type);
            DeepInfra.response(&sent, &mut head).is_some()
        };
        assert!(rewritten(200, "application/json; charset=utf-8"));
        assert!(!rewritten(200, "video/mp4"), "content is relayed as it is");
        assert!(!rewritten(422, "application/json"));
    }

    #[test]
    fn its_job_reads_as_openais() {
        let finished = job(json!({
            "id": "j1", "object": "video.generation.job", "created_at": 1, "status": "succeeded",
            "model": "google/veo-3.1", "data": [{ "url": "https://x/v.mp4" }], "error": null,
        }));
        assert_eq!(
            finished,
            json!({
                "id": "j1", "object": "video.generation.job", "created_at": 1,
                "status": "completed", "model": "google/veo-3.1", "error": null,
            })
        );
        let running = job(json!({ "id": "j2", "status": "running" }));
        assert_eq!(running["status"], "in_progress");
        let failed = job(json!({ "id": "j3", "status": "failed", "error": "blocked" }));
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["error"], json!({ "message": "blocked" }));
        let raw = Bytes::from_static(b"[]");
        assert_eq!(openai_job(raw.clone()).expect("as it is"), raw);
    }

    #[test]
    fn a_reference_image_goes_as_its_image_url() {
        let image_url = |reference: MediaSource| {
            let mut create = video::Create::new("a cat");
            create.reference = Some(Media::new("image/png", reference));
            let mut input = video::Input::Create(create);
            referenced(&mut input).expect("referenced");
            let native = match input {
                video::Input::Create(c) if c.reference.is_none() => c.native,
                _ => None,
            };
            native.and_then(|n| n.fields.get("image_url").cloned())
        };
        let bytes = MediaSource::Bytes(Bytes::from_static(b"png"));
        assert_eq!(image_url(bytes), Some(json!("data:image/png;base64,cG5n")));
        let url = MediaSource::Url("https://x/cat.png".into());
        assert_eq!(image_url(url), Some(json!("https://x/cat.png")));
        let retrieve = || video::Input::Retrieve { id: "j1".into() };
        let mut input = retrieve();
        referenced(&mut input).expect("untouched");
        assert_eq!(input, retrieve());
    }

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
            "a reranker is none it serves"
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
}
