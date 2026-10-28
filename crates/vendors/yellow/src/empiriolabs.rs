//! EmpirioLabs AI: Chat Completions, Responses, Messages, the Gemini API,
//! images, embeddings, speech and decisions, as OpenAI, Anthropic, Google
//! and Respan serve them, images and speech in its dialect.

use base64::Engine as _;
use respan_gateway_conversation::{Media, MediaSource};
use respan_gateway_conversation_http::anthropic::AnthropicMessages;
use respan_gateway_conversation_http::fields::{kept, native_of};
use respan_gateway_conversation_http::gemini::GeminiGenerate;
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_conversation_http::responses::OpenAiResponses;
use respan_gateway_core::{Fault, Filter};
use respan_gateway_decision_http::Decisions;
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::content;
use respan_gateway_http::http::header::{CONTENT_TYPE, HeaderValue};
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::http::response::Parts as ResponseHead;
use respan_gateway_http::path::set_path;
use respan_gateway_media::{image, speech};
use respan_gateway_media_http::openai::{OpenAiEmbeddings, OpenAiImages, OpenAiSpeech};
use respan_gateway_sse as sse;
use serde_json::Value;

use respan_gateway_vendor_kit::{
    Body, BoxFuture, Capability, Dialect, Dialected, Modality, Model, Models, Price, Rewrite,
    Vendor, VendorProvider, built_wrong, http, json, models, time,
};

/// Where its API is.
const BASE_URL: &str = "https://api.empiriolabs.ai/v1";

/// Where its Messages API is: the format adds the `/v1`.
const MESSAGES_URL: &str = "https://api.empiriolabs.ai";

/// Where its Gemini API is.
const GEMINI_URL: &str = "https://api.empiriolabs.ai/v1beta";

/// EmpirioLabs AI.
pub const VENDOR: Vendor = Vendor {
    id: "empiriolabs",
    key_env: &["EMPIRIOLABS_API_KEY"],
    formats: &[
        (&OpenAiChat, Some(BASE_URL)),
        (&OpenAiResponses, Some(BASE_URL)),
        (&AnthropicMessages, Some(MESSAGES_URL)),
        (&GeminiGenerate, Some(GEMINI_URL)),
        (
            &Dialected::new(OpenAiImages, EmpirioLabsImages),
            Some(BASE_URL),
        ),
        (&OpenAiEmbeddings, Some(BASE_URL)),
        (
            &Dialected::new(OpenAiSpeech, EmpirioLabsSpeech),
            Some(BASE_URL),
        ),
        (&Decisions, Some(BASE_URL)),
    ],
    models: list_models,
    ..Vendor::DEFAULT
};

/// EmpirioLabs AI's images, as it departs from OpenAI's: it answers a job
/// unless asked to wait (`sync`), and an image at a signed URL of its own
/// storage unless asked for its bytes: it is asked for both, unless the
/// request says otherwise. An edit is a generation of its `image`s, each a
/// URL or a data URL of its bytes; it takes no mask, which fails
/// `bad_request` before sending. One it did not finish within its wait,
/// about four minutes, it answers as a job, which fails. It streams no
/// images, so it is not asked to; a client that asked still gets a stream,
/// framed at the client edge.
#[derive(Debug, Clone, Copy)]
pub struct EmpirioLabsImages;

impl Dialect for EmpirioLabsImages {
    type Input = image::Input;
    type Output = image::Output;

    fn inputs(&self) -> Option<Box<dyn Filter<image::Input>>> {
        Some(Box::new(taken_image))
    }

    fn request(&self, _: &mut RequestHead) -> Result<Option<Rewrite>, Fault> {
        Ok(Some(Box::new(|body| crate::without(body, "stream"))))
    }
}

/// `input` as it takes an image call: waited for, as bytes, its images
/// named in its `image`, and no mask.
fn taken_image(input: &mut image::Input) -> Result<(), Fault> {
    if input.mask.is_some() {
        return Err(Fault::bad_request(
            "EmpirioLabs AI edits an image with no mask",
        ));
    }
    let mut fields = kept(OpenAiImages::NAME, &input.native)?;
    fields.entry("sync").or_insert(true.into());
    fields.entry("response_format").or_insert("b64_json".into());
    if !input.images.is_empty() {
        let images = input.images.iter().map(|m| Value::from(linked(m)));
        fields.insert("image".into(), images.collect::<Vec<_>>().into());
    }
    input.images.clear();
    input.native = native_of(OpenAiImages::NAME, fields);
    Ok(())
}

/// An image as it takes one: its URL, or a data URL of its bytes.
fn linked(media: &Media) -> String {
    match &media.source {
        MediaSource::Url(url) => url.clone(),
        MediaSource::Bytes(bytes) => format!(
            "data:{};base64,{}",
            media.mime_type,
            base64::engine::general_purpose::STANDARD.encode(bytes)
        ),
    }
}

/// EmpirioLabs AI's speech, as it departs from OpenAI's: its
/// `/audio/speech` answers the audio at a URL of its storage, so it is
/// called at `/audio/speech:stream`, which answers events of the audio.
/// Their `audio.chunk`s are read, whole, as the audio, an `audio.error` as
/// a fault, and a stream without its `audio.done` fails `truncated`. It is
/// not asked for OpenAI's events (`stream_format`); a client that asked
/// still gets a stream, framed at the client edge. It streams at no
/// `speed`, which fails `bad_request` before sending.
#[derive(Debug, Clone, Copy)]
pub struct EmpirioLabsSpeech;

impl Dialect for EmpirioLabsSpeech {
    type Input = speech::Input;
    type Output = speech::Output;

    fn inputs(&self) -> Option<Box<dyn Filter<speech::Input>>> {
        Some(Box::new(taken_speech))
    }

    fn request(&self, head: &mut RequestHead) -> Result<Option<Rewrite>, Fault> {
        set_path(head, "/audio/speech:stream")?;
        Ok(Some(Box::new(|body| crate::without(body, "stream_format"))))
    }

    fn response(&self, _: &RequestHead, head: &mut ResponseHead) -> Option<Rewrite> {
        if !head.status.is_success() || !content::is_stream(head) {
            return None;
        }
        let audio = HeaderValue::from_static("application/octet-stream");
        head.headers.insert(CONTENT_TYPE, audio);
        Some(Box::new(streamed_audio))
    }
}

/// `input` as it streams speech: at no `speed`.
fn taken_speech(input: &mut speech::Input) -> Result<(), Fault> {
    if input.speed.is_some() {
        return Err(Fault::bad_request(
            "EmpirioLabs AI streams no speech at a 'speed'",
        ));
    }
    Ok(())
}

/// The audio of a whole stream of its speech's events.
fn streamed_audio(raw: Bytes) -> Result<Bytes, Fault> {
    let unreadable = |what| Fault::upstream(format!("EmpirioLabs AI's speech {what}"));
    let mut parser = sse::Parser::new(None);
    let mut events = parser.push(raw)?;
    events.extend(parser.finish());
    let mut audio = Vec::new();
    for v in events.iter().filter_map(|e| sse::event_json(e)) {
        match v["type"].as_str() {
            Some("audio.chunk") => {
                let chunk = v["audio_b64"]
                    .as_str()
                    .and_then(|data| base64::engine::general_purpose::STANDARD.decode(data).ok());
                audio.extend(chunk.ok_or_else(|| unreadable("sent audio not in base64"))?);
            }
            Some("audio.done") => return Ok(Bytes::from(audio)),
            Some("audio.error") => {
                let error = &v["error"];
                let message = error["message"].as_str().or_else(|| error.as_str());
                let message = message.or_else(|| v["message"].as_str());
                return Err(unreadable(message.unwrap_or("failed")));
            }
            _ => {}
        }
    }
    Err(Fault::truncated())
}

/// Its models: `GET /models` as OpenAI lists them, each with its category,
/// the endpoints it is served at, its limits, features and price.
fn list_models(p: &VendorProvider) -> BoxFuture<'_, Models> {
    Box::pin(async move {
        let call = http::Request::get("/models").body(Body::default());
        let (answer, _) = p.client()?.send(call.map_err(built_wrong)?).await?;
        Ok(listed(&json(answer).await?))
    })
}

/// The endpoint that serves each category of its models, and the kind it
/// is.
const KINDS: &[(&str, &str, &str)] = &[
    ("text", "POST /v1/chat/completions", "chat"),
    ("embedding", "POST /v1/embeddings", "embedding"),
    ("image", "POST /v1/images/generations", "image"),
    ("audio", "POST /v1/audio/speech:stream", "speech"),
    ("decision", "POST /v1/decisions", "decision"),
];

/// Where it serves chat, which a model of another category may be too.
const CHAT: &str = "POST /v1/chat/completions";

/// The models of its list, each of the kind its category's endpoint is,
/// else a chat if it is served at Chat Completions; one served at neither,
/// such as a video or transcription model, which it answers as jobs, a
/// reranker, a music model, a realtime one or a speech model that does not
/// stream, is none the gateway serves, and is left out.
fn listed(answer: &Value) -> Vec<Model> {
    let entries = answer["data"].as_array().into_iter().flatten();
    entries.filter_map(described).collect()
}

/// One entry of its list: what OpenAI-compatible lists say, read as they
/// say it, and its kind, maker, date, inputs, features and price.
fn described(m: &Value) -> Option<Model> {
    let endpoints = m["supported_endpoints"].as_array();
    let serves = |endpoint: &str| endpoints.is_some_and(|e| e.iter().any(|e| e == endpoint));
    let category = m["category"].as_str();
    let own = KINDS.iter().find(|(c, ..)| Some(*c) == category);
    let kind = match own {
        Some((_, endpoint, kind)) if serves(endpoint) => *kind,
        _ if serves(CHAT) => "chat",
        _ => return None,
    };
    let mut model = models::listed(&Value::Array(vec![without_price(m)])).pop()?;
    model.kind = Some(kind.to_owned());
    model.maker = m["author"].as_str().map(str::to_owned);
    model.created = m["model_released_at"].as_str().and_then(time);
    let inputs = m["input_modalities"].as_array().into_iter().flatten();
    for modality in inputs.filter_map(Value::as_str).filter_map(Modality::parse) {
        model.reads(modality);
    }
    let features = m["features"].as_array().into_iter().flatten();
    for can in features
        .filter_map(Value::as_str)
        .filter_map(Capability::parse)
    {
        model.can(can);
    }
    if m["structured_output"] == "json_schema" {
        model.can(Capability::StructuredOutputs);
    }
    model.price = price(m, kind);
    Some(model)
}

/// An entry without its price, which [`price`] reads.
fn without_price(m: &Value) -> Value {
    let mut m = m.clone();
    if let Some(m) = m.as_object_mut() {
        m.remove("pricing");
    }
    m
}

/// A model's price, for a kind billed by the token: its `pricing` in
/// dollars per token, the first tier's if it has tiers by context length.
/// An image's, by the item, is none.
fn price(m: &Value, kind: &str) -> Option<Price> {
    if kind == "image" {
        return None;
    }
    let pricing = &m["pricing"];
    let pricing = pricing
        .as_array()
        .and_then(|tiers| tiers.first())
        .unwrap_or(pricing);
    let entry = serde_json::json!({ "id": "", "pricing": pricing });
    models::listed(&Value::Array(vec![entry])).pop()?.price
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_core::NativeFields;
    use serde_json::json;

    fn entry(id: &str, category: &str, endpoints: &[&str]) -> Value {
        json!({ "id": id, "object": "model", "category": category,
                "supported_endpoints": endpoints, "author": "acme",
                "pricing": { "prompt": "-1", "completion": "-1" } })
    }

    #[test]
    fn a_models_kind_is_its_categorys_endpoint_else_chat() {
        let answer = json!({ "object": "list", "data": [
            entry("glm-5-3", "text", &["POST /v1/chat/completions", "POST /v1/responses"]),
            entry("text-embedding-v4", "embedding", &["POST /v1/embeddings"]),
            entry("flux-2-klein-4b", "image", &["POST /v1/images/generations"]),
            entry("wan-2-2", "video", &["POST /v1/videos/generations"]),
            entry("qwen-audio-3-0-tts", "audio", &["POST /v1/audio/speech", "POST /v1/audio/speech:stream"]),
            entry("whisper-large-v3-turbo", "transcription", &["POST /v1/audio/transcriptions"]),
            entry("aplomb-1", "decision", &["POST /v1/decisions", "POST /v1/chat/completions"]),
            entry("perplexity-sonar", "research", &["POST /v1/chat/completions", "POST /v1/search"]),
            entry("glm-tts", "audio", &["POST /v1/audio/speech"]),
            entry("qwen3-rerank", "reranker", &["POST /v1/reranks"]),
        ] });
        let kinds: Vec<_> = listed(&answer)
            .into_iter()
            .map(|m| (m.id, m.kind.unwrap_or_default()))
            .collect();
        let said = |id: &str, kind: &str| (id.to_owned(), kind.to_owned());
        assert_eq!(
            kinds,
            [
                said("glm-5-3", "chat"),
                said("text-embedding-v4", "embedding"),
                said("flux-2-klein-4b", "image"),
                said("qwen-audio-3-0-tts", "speech"),
                said("aplomb-1", "decision"),
                said("perplexity-sonar", "chat"),
            ],
            "video, transcription jobs, speech that does not stream and a reranker are none it serves"
        );
    }

    #[test]
    fn its_entry_says_a_models_limits_features_and_price() {
        let chat = json!({ "id": "glm-5-3", "display_name": "GLM 5.3", "author": "zhipu",
            "description": "Coding model", "category": "text",
            "supported_endpoints": ["POST /v1/chat/completions"],
            "context_window": 1000000, "context_length": 1000000, "max_output_tokens": 131072,
            "model_released_at": "2026-08-14", "deprecation_date": "2026-11-17",
            "input_modalities": ["text", "image"], "features": ["reasoning", "function_calling"],
            "structured_output": "json_schema",
            "pricing": [
                { "prompt": "0.0000014", "completion": "0.0000044", "input_cache_read": "0.00000014" },
                { "prompt": "0.0000028", "completion": "0.0000088", "min_context": 256000 },
            ] });
        let image = json!({ "id": "flux-2-klein-4b", "category": "image",
            "supported_endpoints": ["POST /v1/images/generations"],
            "pricing": { "prompt": "0", "completion": "0", "image": "0.0085" } });
        let models = listed(&json!({ "data": [chat, image] }));
        let chat = &models[0];
        assert_eq!(chat.display_name.as_deref(), Some("GLM 5.3"));
        assert_eq!(chat.maker.as_deref(), Some("zhipu"));
        assert_eq!(chat.description.as_deref(), Some("Coding model"));
        assert_eq!(
            (chat.context_window, chat.max_output_tokens),
            (Some(1_000_000), Some(131_072))
        );
        assert_eq!(chat.created.map(|t| t.timestamp()), Some(1_786_665_600));
        assert!(chat.retires_at.is_some());
        assert_eq!(chat.input_modalities, [Modality::Image]);
        assert_eq!(
            chat.capabilities,
            [
                Capability::Reasoning,
                Capability::Tools,
                Capability::StructuredOutputs
            ]
        );
        let price = chat.price.expect("priced by the token");
        assert!((price.input - 1.4).abs() < 1e-9 && (price.output - 4.4).abs() < 1e-9);
        assert_eq!(models[1].price, None, "an image is priced by the item");
    }

    #[test]
    fn an_image_is_asked_for_at_once_and_as_bytes() {
        let mut edit = image::Input::new("make it green");
        edit.images.push(Media::new(
            "image/png",
            MediaSource::Bytes(Bytes::from_static(b"x")),
        ));
        edit.images.push(Media::new(
            "image/png",
            MediaSource::Url("https://x.test/a.png".into()),
        ));
        let mut sent = edit.clone();
        taken_image(&mut sent).expect("taken");
        assert!(
            sent.images.is_empty(),
            "an edit is a generation of its images"
        );
        let fields = sent
            .native
            .as_ref()
            .map(|n| n.fields.clone())
            .unwrap_or_default();
        assert_eq!(fields["sync"], true);
        assert_eq!(fields["response_format"], "b64_json");
        assert_eq!(
            fields["image"],
            json!(["data:image/png;base64,eA==", "https://x.test/a.png"])
        );

        let mut urls = image::Input::new("a fox");
        let mut asked = serde_json::Map::new();
        asked.insert("response_format".into(), "url".into());
        urls.native = Some(NativeFields::new(OpenAiImages::NAME, asked));
        taken_image(&mut urls).expect("taken");
        let fields = urls
            .native
            .as_ref()
            .map(|n| n.fields.clone())
            .unwrap_or_default();
        assert_eq!(fields["response_format"], "url", "a request's own is kept");

        let mut masked = edit.clone();
        masked.mask = Some(edit.images[0].clone());
        assert!(taken_image(&mut masked).is_err());
    }

    fn answer(status: u16, content_type: &str) -> ResponseHead {
        let answer = http::Response::builder()
            .status(status)
            .header("content-type", content_type);
        answer.body(()).expect("head").into_parts().0
    }

    #[test]
    fn speech_is_asked_for_as_a_stream_and_read_whole() {
        let mut head = http::Request::new(()).into_parts().0;
        head.uri = http::Uri::from_static("/audio/speech");
        let rewrite = EmpirioLabsSpeech.request(&mut head).expect("a call");
        assert_eq!(head.uri.path(), "/audio/speech:stream");
        let call = Bytes::from_static(br#"{"input":"Hi.","stream_format":"sse"}"#);
        let call = rewrite.expect("a rewrite")(call).expect("a call");
        assert_eq!(call, &br#"{"input":"Hi."}"#[..]);

        let mut events = answer(200, "text/event-stream");
        let read = EmpirioLabsSpeech.response(&head, &mut events);
        assert_eq!(events.headers[CONTENT_TYPE], "application/octet-stream");
        let stream = "data: {\"type\": \"audio.chunk\", \"seq\": 0, \"audio_b64\": \"SUQz\"}\n\n\
                      data: {\"type\": \"audio.chunk\", \"seq\": 1, \"audio_b64\": \"AAE=\"}\n\n\
                      data: {\"type\":\"audio.done\",\"audio_url\":\"https://media.test/a.mp3\"}\n\n\
                      data: [DONE]\n\n";
        let audio = read.expect("a rewrite")(Bytes::from_static(stream.as_bytes()));
        assert_eq!(audio.expect("audio"), Bytes::from_static(b"ID3\0\x01"));
        let refused = EmpirioLabsSpeech.response(&head, &mut answer(400, "text/event-stream"));
        assert!(refused.is_none(), "a refusal is read as it is");
    }

    #[test]
    fn speech_cut_or_failed_fails() {
        let cut = "data: {\"type\": \"audio.chunk\", \"audio_b64\": \"SUQz\"}\n\n";
        assert!(streamed_audio(Bytes::from_static(cut.as_bytes())).is_err());
        let failed =
            "data: {\"type\": \"audio.error\", \"error\": {\"message\": \"overloaded\"}}\n\n";
        let fault = streamed_audio(Bytes::from_static(failed.as_bytes())).err();
        assert!(fault.is_some_and(|f| f.message().contains("overloaded")));
        let mut fast = speech::Input::new("Hi.");
        fast.speed = Some(1.5);
        assert!(taken_speech(&mut fast).is_err());
    }
}
