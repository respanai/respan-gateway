//! OpenRouter: Chat Completions, embeddings, transcriptions, speech, images
//! and decisions, for many vendors' models; speech, images and decisions in
//! its dialect.

use std::collections::HashMap;

use respan_gateway_conversation::{Media, MediaSource};
use respan_gateway_conversation_http::fields::{kept, native_of};
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::{Fault, Filter};
use respan_gateway_decision_http::Decisions;
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::content;
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::http::response::Parts as ResponseHead;
use respan_gateway_http::path::set_path;
use respan_gateway_media::image;
use respan_gateway_media::speech;
use respan_gateway_media_http::openai::{
    OpenAiEmbeddings, OpenAiImages, OpenAiSpeech, OpenAiTranscriptions,
};
use serde_json::{Map, Value, json};

use respan_gateway_vendor_kit::{
    Body, BoxFuture, Dialect, Dialected, Model, Models, Rewrite, Vendor, VendorProvider,
    built_wrong, http, json, models, time,
};

/// Where its API is.
const BASE_URL: &str = "https://openrouter.ai/api/v1";

/// OpenRouter. Its models' names are `<vendor>/<model>`: a configured
/// provider named `<vendor>` takes the name first.
pub const VENDOR: Vendor = Vendor {
    id: "openrouter",
    key_env: &["OPENROUTER_API_KEY"],
    formats: &[
        (&OpenAiChat, Some(BASE_URL)),
        (&OpenAiEmbeddings, Some(BASE_URL)),
        (&OpenAiTranscriptions, Some(BASE_URL)),
        (
            &Dialected::new(OpenAiSpeech, OpenRouterSpeech),
            Some(BASE_URL),
        ),
        (
            &Dialected::new(OpenAiImages, OpenRouterImages),
            Some(BASE_URL),
        ),
        (
            &Dialected::new(Decisions, OpenRouterDecisions),
            Some(BASE_URL),
        ),
    ],
    models: list_models,
    ..Vendor::DEFAULT
};

/// OpenRouter's speech: its audio is `mp3` or `pcm`, and `pcm` when none is
/// asked, so a call that names none is sent for `mp3`, as OpenAI's is, and
/// any other format fails `bad_request` before sending. It is not asked to
/// stream; a client that asked still gets a stream, framed at the client
/// edge.
#[derive(Debug, Clone, Copy)]
pub struct OpenRouterSpeech;

impl Dialect for OpenRouterSpeech {
    type Input = speech::Input;
    type Output = speech::Output;

    fn inputs(&self) -> Option<Box<dyn Filter<speech::Input>>> {
        Some(Box::new(speech_taken))
    }

    fn request(&self, _: &mut RequestHead) -> Result<Option<Rewrite>, Fault> {
        Ok(Some(Box::new(|body| crate::without(body, "stream_format"))))
    }
}

/// `input` as OpenRouter's speech takes it: for `mp3` or `pcm`, `mp3` if
/// it names none.
fn speech_taken(input: &mut speech::Input) -> Result<(), Fault> {
    match input.audio_format.as_deref() {
        Some("mp3" | "pcm") => Ok(()),
        Some(other) => Err(Fault::bad_request(format!(
            "openrouter speech takes 'mp3' or 'pcm', not '{other}'"
        ))),
        None => {
            input.audio_format = Some("mp3".to_owned());
            Ok(())
        }
    }
}

/// OpenRouter's images: it serves them at `/images`, edits too, which it
/// takes as a generation naming its source images by `input_references`
/// (`{"type": "image_url", "image_url": {"url"}}`); a mask has no place
/// there and fails `bad_request` before sending. It answers whole, in JSON,
/// each image with its `media_type`, and usage as `prompt_tokens` and
/// `completion_tokens` beside its `cost`, read as OpenAI's answer; it is
/// not asked to stream.
#[derive(Debug, Clone, Copy)]
pub struct OpenRouterImages;

impl Dialect for OpenRouterImages {
    type Input = image::Input;
    type Output = image::Output;

    fn inputs(&self) -> Option<Box<dyn Filter<image::Input>>> {
        Some(Box::new(images_taken))
    }

    fn request(&self, head: &mut RequestHead) -> Result<Option<Rewrite>, Fault> {
        set_path(head, "/images")?;
        Ok(Some(Box::new(|body| crate::without(body, "stream"))))
    }

    fn response(&self, _: &RequestHead, head: &mut ResponseHead) -> Option<Rewrite> {
        let whole = head.status.is_success() && !content::is_stream(head);
        whole.then(|| Box::new(as_openais) as Rewrite)
    }
}

/// `input` as OpenRouter's images take it: its images to edit as
/// `input_references`, and no mask.
fn images_taken(input: &mut image::Input) -> Result<(), Fault> {
    if input.mask.is_some() {
        return Err(Fault::bad_request("openrouter images take no 'mask'"));
    }
    if input.images.is_empty() {
        return Ok(());
    }
    let link = |media: &Media| {
        let url = match &media.source {
            MediaSource::Url(url) => url.clone(),
            MediaSource::Bytes(bytes) => {
                use base64::Engine as _;
                let data = base64::engine::general_purpose::STANDARD.encode(bytes);
                format!("data:{};base64,{data}", media.mime_type)
            }
        };
        json!({ "type": "image_url", "image_url": { "url": url } })
    };
    let mut fields = kept(OpenAiImages::NAME, &input.native)?;
    let images: Vec<Value> = input.images.iter().map(link).collect();
    fields.insert("input_references".into(), images.into());
    input.images.clear();
    input.native = native_of(OpenAiImages::NAME, fields);
    Ok(())
}

/// A whole answer as OpenAI's: its images' `media_type` its
/// `output_format`, and its usage's `prompt_tokens` and `completion_tokens`
/// its `input_tokens` and `output_tokens`.
fn as_openais(raw: Bytes) -> Result<Bytes, Fault> {
    let unread = || Fault::upstream("openrouter images answered no JSON object");
    let mut answer: Map<String, Value> = serde_json::from_slice(&raw).map_err(|_| unread())?;
    let mut mime = None;
    let items = answer.get_mut("data").and_then(Value::as_array_mut);
    for item in items.into_iter().flatten().filter_map(Value::as_object_mut) {
        if let Some(Value::String(media_type)) = item.shift_remove("media_type") {
            mime.get_or_insert(media_type);
        }
    }
    if let Some(mime) = mime {
        let format = mime.strip_prefix("image/").unwrap_or(&mime).to_owned();
        answer.entry("output_format").or_insert(format.into());
    }
    if let Some(usage) = answer.get_mut("usage").and_then(Value::as_object_mut) {
        for (theirs, ours) in [
            ("prompt_tokens", "input_tokens"),
            ("completion_tokens", "output_tokens"),
        ] {
            if let Some(count) = usage.shift_remove(theirs) {
                usage.entry(ours).or_insert(count);
            }
        }
    }
    serde_json::to_vec(&answer)
        .map(Bytes::from)
        .map_err(|e| Fault::internal("an images answer is not written").with_source(e))
}

/// OpenRouter's decisions: it serves them as TypeSafe's System One does,
/// at `/systemone`.
#[derive(Debug, Clone, Copy)]
pub struct OpenRouterDecisions;

impl Dialect for OpenRouterDecisions {
    type Input = respan_gateway_decision::Input;
    type Output = respan_gateway_decision::Output;

    fn request(&self, head: &mut RequestHead) -> Result<Option<Rewrite>, Fault> {
        set_path(head, "/systemone")?;
        Ok(None)
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
    ("decisions", "decision"),
];

/// The kinds whose `prompt` and `completion` prices are not by the token: a
/// speech model's are by the character or the second, a transcription
/// model's by the second for some and the token for others, unsaid which;
/// and an image model's are `0` beside its own prices by the image.
const UNPRICED: &[&str] = &["image", "speech", "transcription"];

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
/// outputs say none of [`KINDS`], as a reranker or a video model, is none it
/// serves, and is left out.
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
/// `0` is none. `false` if it is of no kind it serves.
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
                Some("decision"),
            ],
            "a video model, a reranker, or a model of no outputs, is none it serves"
        );
        let makers: Vec<_> = models.iter().map(|m| m.maker.as_deref()).collect();
        assert_eq!(makers[0], Some("openai"));
        assert_eq!(makers[6], Some("typesafe"), "an alias's maker");
        assert_eq!(models[0].price.map(|p| p.input), Some(1.0));
        assert_eq!(models[3].price.map(|p| p.input), Some(0.01));
        let priced: Vec<_> = models.iter().map(|m| m.price.is_some()).collect();
        assert_eq!(priced, [true, true, false, true, false, false, true]);
        assert_eq!(
            (models[2].context_window, models[2].max_output_tokens),
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

    #[test]
    fn images_are_served_at_images_and_an_edit_names_its_sources() {
        let mut head = http::Request::new(()).into_parts().0;
        head.uri = http::Uri::from_static("/images/generations");
        let rewrite = OpenRouterImages.request(&mut head).expect("a call");
        assert_eq!(head.uri.path(), "/images");
        let body = rewrite.expect("a rewrite")(Bytes::from_static(br#"{"stream":true}"#));
        assert_eq!(body.expect("body"), Bytes::from_static(b"{}"));
        let mut input = image::Input::new("add a hat");
        input.images.push(Media::new(
            "image/png",
            MediaSource::Url("https://x/a.png".into()),
        ));
        let mut sent = input.clone();
        images_taken(&mut sent).expect("input");
        assert!(sent.images.is_empty());
        let fields = kept(OpenAiImages::NAME, &sent.native).expect("native");
        assert_eq!(
            fields["input_references"],
            json!([{ "type": "image_url", "image_url": { "url": "https://x/a.png" } }])
        );
        input.mask = Some(input.images[0].clone());
        assert!(images_taken(&mut input).is_err());
    }

    #[test]
    fn an_images_answer_is_read_as_openais() {
        let answer = br#"{"created": 7, "data": [{"b64_json": "SUQz", "media_type": "image/webp"}],
            "usage": {"prompt_tokens": 2, "completion_tokens": 5, "total_tokens": 7, "cost": 0.04}}"#;
        let read = as_openais(Bytes::from_static(answer)).expect("rewritten");
        let read: Value = serde_json::from_slice(&read).expect("json");
        assert_eq!(
            read,
            json!({ "created": 7, "data": [{ "b64_json": "SUQz" }],
                    "output_format": "webp",
                    "usage": { "total_tokens": 7, "cost": 0.04,
                               "input_tokens": 2, "output_tokens": 5 } })
        );
    }

    #[test]
    fn speech_is_asked_for_mp3_unless_it_names_a_format() {
        let mut input = speech::Input::new("hi");
        speech_taken(&mut input).expect("input");
        assert_eq!(input.audio_format.as_deref(), Some("mp3"));
        input.audio_format = Some("pcm".to_owned());
        assert!(speech_taken(&mut input).is_ok());
        input.audio_format = Some("opus".to_owned());
        assert!(speech_taken(&mut input).is_err());
    }
}
