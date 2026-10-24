//! OpenRouter: Chat Completions, embeddings, transcriptions, speech, images,
//! videos and decisions, for many vendors' models; speech, images and
//! decisions in its dialect.

use std::borrow::Cow;
use std::collections::HashMap;

use base64::Engine as _;
use respan_gateway_conversation::Output;
use respan_gateway_conversation::{Media, MediaSource, Usage};
use respan_gateway_conversation_http::dialect::{Call, Dialect, Dialected};
use respan_gateway_conversation_http::fields::{kept, native_of};
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::NativeValue;
use respan_gateway_core::{Fault, Filter};
use respan_gateway_decision_http::Decisions;
use respan_gateway_decision_http::dialect::Dialect as DecisionDialect;
use respan_gateway_decision_http::dialect::Dialected as DecisionDialected;
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::response::Parts as ResponseHead;
use respan_gateway_http::{Decoder, Whole};
use respan_gateway_media::image;
use respan_gateway_media::speech;
use respan_gateway_media_http::dialect::Dialect as MediaDialect;
use respan_gateway_media_http::dialect::Dialected as MediaDialected;
use respan_gateway_media_http::openai::{
    OpenAiEmbeddings, OpenAiImages, OpenAiSpeech, OpenAiTranscriptions,
};
use respan_gateway_media_http::openrouter::OpenRouterVideos;
use serde_json::{Map, Value, json};

use respan_gateway_vendor_kit::{
    Body, BoxFuture, Model, Models, Vendor, VendorProvider, built_wrong, chat, http, json, models,
    time,
};

/// Where its API is.
const BASE_URL: &str = "https://openrouter.ai/api/v1";

/// OpenRouter. Its models' names are `<vendor>/<model>`: a configured
/// provider named `<vendor>` takes the name first.
pub const VENDOR: Vendor = Vendor {
    id: "openrouter",
    key_env: &["OPENROUTER_API_KEY"],
    formats: &[
        (&Dialected::new(OpenAiChat, OpenRouter), Some(BASE_URL)),
        (&OpenAiEmbeddings, Some(BASE_URL)),
        (&OpenAiTranscriptions, Some(BASE_URL)),
        (
            &MediaDialected::new(OpenAiSpeech, OpenRouterSpeech),
            Some(BASE_URL),
        ),
        (
            &MediaDialected::new(OpenAiImages, OpenRouterImages),
            Some(BASE_URL),
        ),
        (&OpenRouterVideos, Some(BASE_URL)),
        (
            &DecisionDialected::new(Decisions, OpenRouterDecisions),
            Some(BASE_URL),
        ),
    ],
    models: list_models,
    ..Vendor::DEFAULT
};

/// OpenRouter's Chat Completions: a choice's `native_finish_reason` only
/// describes it.
#[derive(Debug, Clone, Copy)]
pub struct OpenRouter;

impl Dialect for OpenRouter {
    fn output(&self, call: &Call) -> Option<Box<dyn Filter<Output>>> {
        chat::read(call, &["native_finish_reason"], false)
    }
}

/// OpenRouter's speech: its audio is `mp3` or `pcm`, and `pcm` when none is
/// asked, so a call that names none is sent for `mp3`, as OpenAI's is, and
/// any other format fails `bad_request` before sending. It is not asked to
/// stream.
#[derive(Debug, Clone, Copy)]
pub struct OpenRouterSpeech;

impl MediaDialect<speech::Input, speech::Output> for OpenRouterSpeech {
    fn input<'a>(&self, input: &'a speech::Input) -> Result<Cow<'a, speech::Input>, Fault> {
        match input.audio_format.as_deref() {
            Some("mp3" | "pcm") => Ok(Cow::Borrowed(input)),
            Some(other) => Err(Fault::bad_request(format!(
                "openrouter speech takes 'mp3' or 'pcm', not '{other}'"
            ))),
            None => {
                let mut input = input.clone();
                input.audio_format = Some("mp3".to_owned());
                Ok(Cow::Owned(input))
            }
        }
    }

    fn streams(&self, _: bool) -> bool {
        false
    }
}

/// OpenRouter's images: it serves them at `/images`, edits too, which it
/// takes as a generation naming its source images by `input_references`
/// (`{"type": "image_url", "image_url": {"url"}}`); a mask has no place
/// there and fails `bad_request` before sending. It answers whole, in JSON,
/// each image with its `media_type`, and usage as `prompt_tokens` and
/// `completion_tokens` beside its `cost`; it is not asked to stream.
#[derive(Debug, Clone, Copy)]
pub struct OpenRouterImages;

impl MediaDialect<image::Input, image::Output> for OpenRouterImages {
    fn input<'a>(&self, input: &'a image::Input) -> Result<Cow<'a, image::Input>, Fault> {
        if input.mask.is_some() {
            return Err(Fault::bad_request("openrouter images take no 'mask'"));
        }
        if input.images.is_empty() {
            return Ok(Cow::Borrowed(input));
        }
        let link = |media: &Media| {
            let url = match &media.source {
                MediaSource::Url(url) => url.clone(),
                MediaSource::Bytes(bytes) => format!(
                    "data:{};base64,{}",
                    media.mime_type,
                    base64::engine::general_purpose::STANDARD.encode(bytes)
                ),
            };
            json!({ "type": "image_url", "image_url": { "url": url } })
        };
        let mut fields = kept(OpenAiImages::NAME, &input.native)?;
        let images: Vec<Value> = input.images.iter().map(link).collect();
        fields.insert("input_references".into(), images.into());
        let mut input = input.clone();
        input.images.clear();
        input.native = native_of(OpenAiImages::NAME, fields);
        Ok(Cow::Owned(input))
    }

    fn path(&self) -> Option<&'static str> {
        Some("/images")
    }

    fn read_response(&self, head: &ResponseHead) -> Option<Box<dyn Decoder<image::Output>>> {
        head.status
            .is_success()
            .then(|| Box::new(Whole::new(images)) as _)
    }

    fn streams(&self, _: bool) -> bool {
        false
    }
}

/// The events of a whole answer: its [`image::Start`], an [`image::Image`]
/// of each item of `data`, and its usage, `cost` and the rest native.
fn images(raw: Bytes, out: &mut Vec<image::Output>) -> Result<(), Fault> {
    let answer: Value = serde_json::from_slice(&raw)
        .map_err(|_| Fault::upstream("openrouter images answered no JSON"))?;
    let items = answer["data"]
        .as_array()
        .ok_or_else(|| Fault::upstream("openrouter images answered no 'data'"))?;
    out.push(image::Output::Start(image::Start::new(
        answer["created"].as_u64(),
    )));
    for item in items {
        let bytes = item["b64_json"]
            .as_str()
            .and_then(|data| base64::engine::general_purpose::STANDARD.decode(data).ok())
            .ok_or_else(|| Fault::upstream("openrouter images answered no base64 'b64_json'"))?;
        let mime = item["media_type"].as_str().unwrap_or("image/png");
        let media = Media::new(mime, MediaSource::Bytes(Bytes::from(bytes)));
        out.push(image::Output::Image(image::Image::new(media)));
    }
    if let Some(usage) = answer["usage"].as_object() {
        let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
        out.push(image::Output::Usage(Usage::new(
            count("prompt_tokens"),
            count("completion_tokens"),
        )));
        const READ: &[&str] = &["prompt_tokens", "completion_tokens", "total_tokens"];
        let rest: Map<String, Value> = usage
            .iter()
            .filter(|(k, _)| !READ.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if !rest.is_empty() {
            let native = NativeValue::new(OpenAiImages::NAME, json!({ "usage": rest }));
            out.push(image::Output::Native(native));
        }
    }
    Ok(())
}

/// OpenRouter's decisions: it serves them as TypeSafe's System One does,
/// at `/systemone`.
#[derive(Debug, Clone, Copy)]
pub struct OpenRouterDecisions;

impl DecisionDialect for OpenRouterDecisions {
    fn path(&self) -> Option<&'static str> {
        Some("/systemone")
    }
}

/// The kind each of its outputs says a model is, the first it has: one
/// that writes text chats, whatever else it writes.
const KINDS: &[(&str, &str)] = &[
    ("text", "chat"),
    ("image", "image"),
    ("embeddings", "embedding"),
    ("speech", "speech"),
    ("transcription", "transcription"),
    ("video", "video"),
    ("decisions", "decision"),
];

/// The kinds whose `prompt` and `completion` prices are not by the token: a
/// speech model's are by the character or the second, a transcription
/// model's by the second for some and the token for others, unsaid which;
/// an image model's are `0` beside its own prices by the image, and a video
/// model's `0` alone.
const UNPRICED: &[&str] = &["image", "speech", "transcription", "video"];

/// Its models: `GET /models?output_modalities=all`, every model of every
/// output, each with what OpenAI-compatible lists add.
fn list_models(p: &VendorProvider) -> BoxFuture<'_, Models> {
    Box::pin(async move {
        let call = http::Request::get("/models?output_modalities=all").body(Body::default());
        let (answer, _) = p.client()?.send(call.map_err(built_wrong)?).await?;
        Ok(listed(&json(answer).await?))
    })
}

/// The models of its list, each of the kind its outputs say; one whose
/// outputs say none of [`KINDS`], as a reranker, is none it serves, and is
/// left out.
fn listed(answer: &Value) -> Vec<Model> {
    let entries = answer["data"].as_array().into_iter().flatten();
    let by_id: HashMap<&str, &Value> = entries
        .filter_map(|m| Some((m["id"].as_str()?, m)))
        .collect();
    let mut listed = models::listed(answer);
    listed.retain_mut(|model| {
        let Some(m) = by_id.get(model.id.as_str()) else {
            return false;
        };
        described(model, m)
    });
    listed
}

/// What its entry `m` adds to `model`: its kind, its maker, the first part
/// of its id (an alias's after its `~`), and when it expires; a limit of
/// `0`, as a video model's, is none. `false` if it is of no kind it serves.
fn described(model: &mut Model, m: &Value) -> bool {
    let outputs = m["architecture"]["output_modalities"].as_array();
    let writes = |output: &str| outputs.is_some_and(|o| o.iter().any(|w| w == output));
    let Some((_, kind)) = KINDS.iter().find(|(output, _)| writes(output)) else {
        return false;
    };
    model.kind = Some((*kind).to_owned());
    model.maker = model
        .id
        .trim_start_matches('~')
        .split_once('/')
        .map(|(maker, _)| maker.to_owned());
    model.retires_at = m["expiration_date"].as_str().and_then(time);
    model.context_window = model.context_window.filter(|n| *n > 0);
    model.max_output_tokens = model.max_output_tokens.filter(|n| *n > 0);
    if UNPRICED.contains(kind) {
        model.price = None;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_models_outputs_say_its_kind_and_a_reranker_is_left_out() {
        let card = |id: &str, out: Value, prompt: &str| {
            json!({ "id": id, "context_length": 0, "expiration_date": null,
                    "architecture": { "output_modalities": out },
                    "top_provider": { "max_completion_tokens": 0 },
                    "pricing": { "prompt": prompt, "completion": "0" } })
        };
        let answer = json!({ "data": [
            card("openai/gpt-x", json!(["text"]), "0.000001"),
            card("google/nano-banana", json!(["image", "text"]), "0.000001"),
            card("black-forest-labs/flux", json!(["image"]), "0"),
            card("nvidia/embed", json!(["embeddings"]), "0.00000001"),
            card("hexgrad/kokoro", json!(["speech"]), "0.00000062"),
            card("openai/whisper", json!(["transcription"]), "0.00000333"),
            card("x-ai/grok-video", json!(["video"]), "0"),
            card("voyageai/rerank", json!(["rerank"]), "0"),
            card("~typesafe/jev-latest", json!(["decisions"]), "0"),
            json!({ "id": "a/bare" }),
        ] });
        let models = listed(&answer);
        let kinds: Vec<_> = models.iter().map(|m| m.kind.as_deref()).collect();
        assert_eq!(
            kinds,
            [
                Some("chat"),
                Some("chat"),
                Some("image"),
                Some("embedding"),
                Some("speech"),
                Some("transcription"),
                Some("video"),
                Some("decision"),
            ],
            "a reranker, or a model of no outputs, is none it serves"
        );
        let makers: Vec<_> = models.iter().map(|m| m.maker.as_deref()).collect();
        assert_eq!(makers[0], Some("openai"));
        assert_eq!(makers[7], Some("typesafe"), "an alias's maker");
        assert_eq!(models[0].price.map(|p| p.input), Some(1.0));
        assert_eq!(models[3].price.map(|p| p.input), Some(0.01));
        let priced: Vec<_> = models.iter().map(|m| m.price.is_some()).collect();
        assert_eq!(priced, [true, true, false, true, false, false, false, true]);
        assert_eq!(
            (models[6].context_window, models[6].max_output_tokens),
            (None, None)
        );
    }

    #[test]
    fn a_models_expiry_is_when_it_retires() {
        let answer = json!({ "data": [{ "id": "poolside/laguna", "expiration_date": "2026-10-31",
            "architecture": { "output_modalities": ["text"] } }] });
        let retires = listed(&answer)[0].retires_at.map(|t| t.timestamp());
        assert_eq!(retires, Some(1_793_404_800));
    }

    fn decode(body: &str) -> Vec<image::Output> {
        let head = http::Response::builder()
            .body(())
            .expect("head")
            .into_parts()
            .0;
        let mut decoder = OpenRouterImages.read_response(&head).expect("read");
        let mut out = Vec::new();
        decoder
            .decode(Bytes::copy_from_slice(body.as_bytes()), &mut out)
            .expect("decode");
        decoder.end(Ok(()), &mut out).expect("end");
        out
    }

    #[test]
    fn images_are_served_at_images_and_an_edit_names_its_sources() {
        assert_eq!(OpenRouterImages.path(), Some("/images"));
        let mut input = image::Input::new("add a hat");
        input.images.push(Media::new(
            "image/png",
            MediaSource::Url("https://x/a.png".into()),
        ));
        let sent = OpenRouterImages.input(&input).expect("input");
        assert!(sent.images.is_empty());
        let fields = kept(OpenAiImages::NAME, &sent.native).expect("native");
        assert_eq!(
            fields["input_references"],
            json!([{ "type": "image_url", "image_url": { "url": "https://x/a.png" } }])
        );
        input.mask = Some(input.images[0].clone());
        assert!(OpenRouterImages.input(&input).is_err());
    }

    #[test]
    fn an_images_answer_is_its_images_and_usage() {
        let out = decode(
            r#"{"created": 7, "data": [{"b64_json": "SUQz", "media_type": "image/webp"}],
                "usage": {"prompt_tokens": 2, "completion_tokens": 5, "total_tokens": 7, "cost": 0.04}}"#,
        );
        assert_eq!(out.len(), 4);
        let image::Output::Image(first) = &out[1] else {
            panic!("no image")
        };
        assert_eq!(first.media.mime_type, "image/webp");
        assert!(
            matches!(&out[2], image::Output::Usage(u) if u.input_tokens == 2 && u.output_tokens == 5)
        );
        assert!(matches!(&out[3], image::Output::Native(_)));
    }

    #[test]
    fn speech_is_asked_for_mp3_unless_it_names_a_format() {
        let mut input = speech::Input::new("hi");
        let sent = OpenRouterSpeech.input(&input).expect("input");
        assert_eq!(sent.audio_format.as_deref(), Some("mp3"));
        input.audio_format = Some("pcm".to_owned());
        assert!(OpenRouterSpeech.input(&input).is_ok());
        input.audio_format = Some("opus".to_owned());
        assert!(OpenRouterSpeech.input(&input).is_err());
    }
}
