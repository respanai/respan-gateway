//! DeepInfra: Chat Completions, embeddings, speech and transcriptions, as
//! OpenAI serves them, images and videos in its dialect, and decisions.

use respan_gateway_conversation_http::fields::media_url;
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::{Fault, Filter};
use respan_gateway_decision_http::Decisions;
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::Request;
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::http::response::Parts as ResponseHead;
use respan_gateway_http::path::{path_ends_with, set_path};
use respan_gateway_media::{image, video};
use respan_gateway_media_http::openai::{
    OpenAiEmbeddings, OpenAiImages, OpenAiSpeech, OpenAiTranscriptions, OpenAiVideos,
};
use respan_gateway_media_http::{Natives, named_aspect};

use serde_json::{Map, Value, json};

use respan_gateway_vendor_kit::{
    Body, BoxFuture, Dialect, Dialected, Model, Models, Price, Rewrite, Vendor, VendorProvider,
    built_wrong, http, json,
};

/// Where its API is.
const BASE_URL: &str = "https://api.deepinfra.com/v1/openai";

/// Where its decisions and its images are: not under `/openai`.
const DECISIONS_BASE_URL: &str = "https://api.deepinfra.com/v1";

/// DeepInfra.
pub const VENDOR: Vendor = Vendor {
    id: "deepinfra",
    key_env: &["DEEPINFRA_API_KEY"],
    formats: &[
        (&OpenAiChat, Some(BASE_URL)),
        (
            &Dialected::new(OpenAiImages, DeepInfraImages),
            Some(DECISIONS_BASE_URL),
        ),
        (&OpenAiEmbeddings, Some(BASE_URL)),
        (&OpenAiSpeech, Some(BASE_URL)),
        (&OpenAiTranscriptions, Some(BASE_URL)),
        (&Dialected::new(OpenAiVideos, DeepInfra), Some(BASE_URL)),
        (&Decisions, Some(DECISIONS_BASE_URL)),
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

    fn response(&self, _: &Request<Bytes>, head: &mut ResponseHead) -> Option<Rewrite> {
        crate::json_answer(head).then(|| Box::new(openai_job) as Rewrite)
    }
}

/// The edit models whose input has a list of images, each with the field
/// that list is and how it takes a size. Its OpenAI-compatible edits take one
/// `image` file, which they put in a model's `image` field and no other, so
/// these are called at `/inference/{model}` with their own fields.
const LISTED: &[(&str, &str, Sizer)] = &[
    ("Wan-AI/Wan2.6-Image-Edit", "image_urls", alibaba_size),
    ("Wan-AI/Wan2.7-Image-Edit", "image_urls", alibaba_size),
    ("Qwen/Qwen-Image-Edit", "image_urls", alibaba_size),
    ("Qwen/Qwen-Image-Edit-Max", "image_urls", alibaba_size),
    ("PrunaAI/p-image-Edit", "images", pruna_size),
];

/// How a model takes a size: it writes its fields given a size `WxH`.
type Sizer = fn(&mut Map<String, Value>, &str) -> Result<(), Fault>;

/// Alibaba's models take a size as `W*H`.
fn alibaba_size(fields: &mut Map<String, Value>, size: &str) -> Result<(), Fault> {
    fields.insert("size".into(), size.replace('x', "*").into());
    Ok(())
}

/// The aspect ratios Pruna draws, as it names them, each a width to a height.
const PRUNA_RATIOS: [(&str, u32, u32); 7] = [
    ("1:1", 1, 1),
    ("16:9", 16, 9),
    ("9:16", 9, 16),
    ("4:3", 4, 3),
    ("3:4", 3, 4),
    ("3:2", 3, 2),
    ("2:3", 2, 3),
];

/// Pruna's models take an `aspect_ratio`, the nearest it draws within 5%, else
/// the size is refused, 400; `auto` sends none.
fn pruna_size(fields: &mut Map<String, Value>, size: &str) -> Result<(), Fault> {
    if let Some(((ratio, ..), _)) = named_aspect(size, &PRUNA_RATIOS, "Pruna")? {
        fields.insert("aspect_ratio".into(), ratio.into());
    }
    Ok(())
}

/// The row of [`LISTED`] for a model.
fn listed_model(model: Option<&str>) -> Option<&'static (&'static str, &'static str, Sizer)> {
    LISTED.iter().find(|(id, ..)| Some(*id) == model)
}

/// The row of [`LISTED`] for the model an OpenAI-shaped JSON `body` names.
fn listed_in(body: &[u8]) -> Option<&'static (&'static str, &'static str, Sizer)> {
    let call: Value = serde_json::from_slice(body).ok()?;
    listed_model(call["model"].as_str())
}

/// DeepInfra's images, at its `/v1`: a generation, and an edit of most
/// models, under `/openai` as OpenAI's. An edit of a model of [`LISTED`] is
/// a call to `/inference/{model}`, sent as JSON with the model's own fields:
/// its images as a list in the field it is, its count as `num_images`, its
/// size as it takes it, and the answer's `images` (URLs, or data URLs of
/// the bytes) read as OpenAI's `data`. The mask is dropped, as they take none.
#[derive(Debug, Clone, Copy)]
pub struct DeepInfraImages;

impl Dialect for DeepInfraImages {
    type Input = image::Input;
    type Output = image::Output;

    fn inputs(&self) -> Option<Box<dyn Filter<image::Input>>> {
        Some(Box::new(|input: &mut image::Input| {
            if listed_model(input.model.as_deref()).is_some() {
                crate::as_urls(&mut input.images);
            }
            Ok(())
        }))
    }

    fn route(&self, head: &mut RequestHead, body: &Bytes) -> Result<(), Fault> {
        if path_ends_with(&head.uri, "/images/edits")
            && let Some((model, ..)) = listed_in(body)
        {
            return set_path(head, &format!("/inference/{model}"));
        }
        Ok(())
    }

    fn request(&self, head: &mut RequestHead) -> Result<Option<Rewrite>, Fault> {
        if head.uri.path().starts_with("/inference/") {
            return Ok(Some(Box::new(inferred)));
        }
        let path = format!("/openai{}", head.uri.path());
        set_path(head, &path)?;
        Ok(None)
    }

    fn response(&self, sent: &Request<Bytes>, head: &mut ResponseHead) -> Option<Rewrite> {
        let edit = path_ends_with(sent.uri(), "/images/edits");
        let listed = edit && listed_in(sent.body()).is_some();
        (listed && crate::json_answer(head)).then(|| Box::new(drawn) as Rewrite)
    }
}

/// What an OpenAI edit has no field for in the call of a listed model: its
/// own, dropped.
const OPENAI_ONLY: &[&str] = &[
    "model",
    "stream",
    "mask",
    "response_format",
    "user",
    "quality",
    "output_format",
];

/// An OpenAI edit as its model's own input.
fn inferred(raw: Bytes) -> Result<Bytes, Fault> {
    let unwritten = |e| Fault::internal("an image edit is no JSON object").with_source(e);
    let mut call: Map<String, Value> = serde_json::from_slice(&raw).map_err(unwritten)?;
    let Some((_, list, sized)) = listed_model(call["model"].as_str()) else {
        return Ok(raw);
    };
    let urls = call.shift_remove("images").and_then(|images| match images {
        Value::Array(images) => Some(images),
        _ => None,
    });
    let urls = urls.into_iter().flatten();
    let urls: Vec<Value> = urls.filter_map(|i| i.get("image_url").cloned()).collect();
    let count = call.shift_remove("n").unwrap_or(1.into());
    let size = call.get("size").and_then(Value::as_str).map(str::to_owned);
    call.retain(|name, _| !OPENAI_ONLY.contains(&name.as_str()) && name != "size");
    call.insert(list.to_string(), urls.into());
    call.insert("num_images".into(), count);
    if let Some(size) = size {
        sized(&mut call, &size)?;
    }
    serde_json::to_vec(&call)
        .map(Bytes::from)
        .map_err(unwritten)
}

/// Its answer to an edit, `{"images": [..]}`, as OpenAI's `{"data": [..]}`:
/// a data URL is the image's bytes as `b64_json`, any other URL the image's.
fn drawn(raw: Bytes) -> Result<Bytes, Fault> {
    let Ok(answer) = serde_json::from_slice::<Value>(&raw) else {
        return Ok(raw);
    };
    let image = |url: &Value| match url.as_str()?.split_once(";base64,") {
        Some((_, bytes)) => Some(json!({ "b64_json": bytes })),
        None => Some(json!({ "url": url })),
    };
    let images = answer["images"].as_array().into_iter().flatten();
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let answer =
        json!({ "created": created, "data": images.filter_map(image).collect::<Vec<_>>() });
    serde_json::to_vec(&answer)
        .map(Bytes::from)
        .map_err(|e| Fault::internal("an image answer is no JSON").with_source(e))
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
    create.native = OpenAiVideos::with_fields(&create.native, |fields| {
        fields.entry("image_url").or_insert_with(|| url.into());
    });
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
        let sent = http::Request::new(Bytes::new());
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
            let video::Input::Create(c) = input else {
                return None;
            };
            if c.reference.is_some() {
                return None;
            }
            OpenAiVideos::fields(&c.native).get("image_url").cloned()
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

    fn edit_body(model: &str) -> Bytes {
        let call = json!({
            "model": model, "prompt": "make it blue", "n": 2, "size": "1024x768",
            "mask": { "image_url": "data:image/png;base64,bWFzaw==" }, "seed": 7,
            "images": [{ "image_url": "https://x/a.png" }, { "image_url": "data:image/png;base64,cG5n" }],
        });
        Bytes::from(call.to_string())
    }

    fn inferred_as(model: &str) -> Value {
        let native = inferred(edit_body(model)).expect("native");
        serde_json::from_slice(&native).expect("JSON")
    }

    #[test]
    fn an_alibaba_edit_names_its_images_in_image_urls() {
        let urls = json!(["https://x/a.png", "data:image/png;base64,cG5n"]);
        assert_eq!(
            inferred_as("Wan-AI/Wan2.6-Image-Edit"),
            json!({ "prompt": "make it blue", "num_images": 2, "size": "1024*768", "seed": 7, "image_urls": urls })
        );
    }

    #[test]
    fn a_pruna_edit_names_its_images_in_images_and_its_size_as_an_aspect_ratio() {
        let urls = json!(["https://x/a.png", "data:image/png;base64,cG5n"]);
        assert_eq!(
            inferred_as("PrunaAI/p-image-Edit"),
            json!({ "prompt": "make it blue", "num_images": 2, "aspect_ratio": "4:3", "seed": 7, "images": urls })
        );
    }

    #[test]
    fn a_size_of_no_aspect_ratio_pruna_draws_is_refused() {
        let body = |size: &str| {
            let call = json!({ "model": "PrunaAI/p-image-Edit", "prompt": "a", "size": size, "images": [] });
            Bytes::from(call.to_string())
        };
        assert!(inferred(body("1024x256")).is_err());
        let auto = inferred(body("auto")).expect("native");
        let auto: Value = serde_json::from_slice(&auto).expect("JSON");
        assert_eq!(auto.get("aspect_ratio"), None);
    }

    #[test]
    fn an_edit_of_a_listed_model_goes_to_its_inference_and_the_rest_under_openai() {
        let call = |path: &str| {
            http::Request::post(path)
                .body(())
                .expect("call")
                .into_parts()
                .0
        };
        let mut edit = call("/images/edits");
        DeepInfraImages
            .route(&mut edit, &edit_body("PrunaAI/p-image-Edit"))
            .expect("routed");
        assert!(DeepInfraImages.request(&mut edit).expect("sent").is_some());
        assert_eq!(edit.uri.path(), "/inference/PrunaAI/p-image-Edit");
        let others = [
            (
                "/images/edits",
                edit_body("black-forest-labs/FLUX.1-Kontext-dev"),
            ),
            ("/images/generations", edit_body("PrunaAI/p-image-Edit")),
        ];
        for (path, body) in others {
            let mut head = call(path);
            DeepInfraImages.route(&mut head, &body).expect("routed");
            assert!(DeepInfraImages.request(&mut head).expect("sent").is_none());
            assert_eq!(head.uri.path(), format!("/openai{path}"));
        }
    }

    #[test]
    fn the_bytes_of_a_listed_edits_images_are_sent_as_data_urls() {
        let edit = |model: &str| {
            let mut input = image::Input::new("make it blue");
            input.model = Some(model.into());
            let bytes = MediaSource::Bytes(Bytes::from_static(b"png"));
            input.images.push(Media::new("image/png", bytes));
            input
        };
        let sources = |model: &str| {
            let mut input = edit(model);
            let mut filter = DeepInfraImages.inputs().expect("a filter");
            let mut out = Vec::new();
            filter.edit(input.clone(), &mut out).expect("filtered");
            input = out.remove(0);
            input.images[0].source.clone()
        };
        let data = MediaSource::Url("data:image/png;base64,cG5n".into());
        assert_eq!(sources("Wan-AI/Wan2.6-Image-Edit"), data);
        let bytes = MediaSource::Bytes(Bytes::from_static(b"png"));
        assert_eq!(sources("black-forest-labs/FLUX.1-Kontext-dev"), bytes);
    }

    #[test]
    fn its_images_read_as_openais_data() {
        let answer = json!({ "request_id": "r", "images": ["data:image/jpeg;base64,/9j/", "https://x/i.png"] });
        let read = drawn(Bytes::from(answer.to_string())).expect("an answer");
        let read: Value = serde_json::from_slice(&read).expect("JSON");
        assert_eq!(
            read["data"],
            json!([{ "b64_json": "/9j/" }, { "url": "https://x/i.png" }])
        );
        let sent = http::Request::post("/images/edits")
            .body(edit_body("Wan-AI/Wan2.6-Image-Edit"))
            .expect("sent");
        let answers = |status, content_type| {
            let mut head = head(status, content_type);
            DeepInfraImages.response(&sent, &mut head).is_some()
        };
        assert!(answers(200, "application/json"));
        assert!(!answers(400, "application/json"));
        let generated = http::Request::post("/images/generations")
            .body(edit_body("Wan-AI/Wan2.6-Image-Edit"))
            .expect("sent");
        let mut head = head(200, "application/json");
        assert!(DeepInfraImages.response(&generated, &mut head).is_none());
    }
}
