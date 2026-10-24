//! Amazon Bedrock's InvokeModel, over HTTP: a provider format of the
//! [`embedding`](respan_gateway_media::embedding) kind, [`BedrockEmbeddings`].
//! A provider's base URL is its region's runtime,
//! `https://bedrock-runtime.{region}.amazonaws.com`, and its transport the
//! key: a Bedrock API key as a bearer token, or a signature its host adds.

use std::pin::Pin;
use std::task::{Context, Poll};

use futures::channel::oneshot;
use futures::future::BoxFuture;
use futures::{FutureExt, Sink, SinkExt, StreamExt, TryStreamExt, stream};
use respan_gateway_conversation::Usage;
use respan_gateway_core::{
    Connection, EventSink, EventStream, Extensions, Fault, Format, Provider,
};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::HeaderValue;
use respan_gateway_http::http::header::CONTENT_TYPE;
use respan_gateway_http::http::response::Parts as ResponseHead;
use respan_gateway_http::{Connect, Decoder, Upstream, Whole};
use respan_gateway_media::embedding::{Embedding, Input, Item, Output, Start};
use serde_json::{Value, json};

use crate::edge::{bind_provider, escape, sends};
use crate::openai::OpenAiEmbeddings;

const NAME: &str = "bedrock-embeddings";
/// The header Bedrock counts a call's input tokens in.
const INPUT_TOKENS: &str = "x-amzn-bedrock-input-token-count";
/// What Cohere embeds a text as unless the client says.
const SEARCH_DOCUMENT: &str = "search_document";
/// What Nova embeds for unless the client says.
const GENERIC_INDEX: &str = "GENERIC_INDEX";
/// The most calls of one request in flight: those of the models that embed
/// one input per call.
const IN_FLIGHT: usize = 8;

/// Amazon Bedrock's InvokeModel for embeddings, `POST
/// /model/{model}/invoke`, which needs a model: Cohere Embed
/// (`cohere.embed-*`), Amazon Titan Embeddings (`amazon.titan-embed-text-*`,
/// `amazon.titan-embed-image-v1`), Amazon Nova Multimodal Embeddings
/// (`amazon.nova-2-multimodal-embeddings-v1:0`) and TwelveLabs Marengo
/// Embed 3.0 (`twelvelabs.marengo-embed-3-0-v1:0`), an inference profile's
/// id (`us.cohere.embed-v4:0`) as it is.
///
/// Cohere embeds every text in one call, as a `search_document` unless the
/// client's `input_type` (an [`OpenAiEmbeddings`] field) says otherwise,
/// and an item of parts as one input: v3 one image, v4 texts and images
/// together; v4 takes `dimensions` as `output_dimension`. The others embed
/// one input per call, so a request of several is as many calls, a few at
/// a time: Titan a text, or Multimodal's text and image together; Nova
/// each text and each part (a text, image, audio or video), `dimensions` as
/// its `embeddingDimension` and the client's `input_type` as its
/// `embeddingPurpose`; Marengo a text, an image, or the two together.
/// Vectors are asked for as floats and come in the request's order, each
/// part's of Nova's in turn. Usage is the input tokens Bedrock counts:
/// Titan's `inputTextTokenCount`, else the `x-amzn-bedrock-input-token-count`
/// header.
///
/// Media in a part is a base64 data URL (`{"type": "image", "url":
/// "data:image/png;base64,..."}`), as Bedrock takes media as bytes. Token
/// ids, media by URL, parts a model does not read, a `dimensions` it takes
/// none of, and fields of another format are refused, 400, before any
/// call. What Bedrock answers beyond the vectors is dropped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BedrockEmbeddings;

impl BedrockEmbeddings {
    /// The format's name, `bedrock-embeddings`.
    pub const NAME: &'static str = NAME;

    /// Whether it embeds with `model`, a model's id or an inference
    /// profile's.
    pub fn embeds(model: &str) -> bool {
        Family::of(model).is_some()
    }
}

/// A model family Bedrock embeds with, as its id says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Cohere { v4: bool },
    TitanText { v2: bool },
    TitanMultimodal,
    Nova,
    Marengo,
}

impl Family {
    fn of(model: &str) -> Option<Family> {
        let has = |name: &str| model.contains(name);
        Some(match () {
            _ if has("cohere.embed-v4") => Family::Cohere { v4: true },
            _ if has("cohere.embed-") => Family::Cohere { v4: false },
            _ if has("amazon.titan-embed-image-") => Family::TitanMultimodal,
            _ if has("amazon.titan-embed-text-v2") => Family::TitanText { v2: true },
            _ if has("amazon.titan-embed-text-v1") || has("amazon.titan-embed-g1-text") => {
                Family::TitanText { v2: false }
            }
            _ if has("amazon.nova-") && has("-embeddings") => Family::Nova,
            _ if has("twelvelabs.marengo-embed-3") => Family::Marengo,
            _ => return None,
        })
    }
}

/// What a part of an item holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part<'a> {
    Text(&'a str),
    Media(Media<'a>),
}

/// A part's media, from its data URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Media<'a> {
    of: Sense,
    /// Its type, such as `image/png`.
    mime: &'a str,
    /// Its bytes, in base64.
    data: &'a str,
    /// The whole data URL.
    url: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sense {
    Image,
    Audio,
    Video,
}

impl Sense {
    fn name(self) -> &'static str {
        match self {
            Sense::Image => "images",
            Sense::Audio => "audio",
            Sense::Video => "video",
        }
    }
}

/// A part as a client writes one: `{"type": "text", "text"}`, or media of
/// its type (`image`, `image_url`, `audio`, `video`) at `url` or
/// `{type}.url`, a base64 data URL.
fn part(v: &Value) -> Result<Part<'_>, Fault> {
    let kind = v["type"].as_str().unwrap_or_default();
    let of = match kind {
        "text" => {
            let text = v["text"].as_str();
            return text
                .map(Part::Text)
                .ok_or_else(|| Fault::bad_request("a text part needs its `text`"));
        }
        "image" | "image_url" | "input_image" => Sense::Image,
        "audio" | "input_audio" => Sense::Audio,
        "video" | "video_url" => Sense::Video,
        _ => {
            return Err(Fault::bad_request(format!(
                "Bedrock embeds no part of type {kind:?}"
            )));
        }
    };
    let url = [&v["url"], &v[kind], &v[kind]["url"], &v["image_url"]["url"]]
        .into_iter()
        .find_map(Value::as_str)
        .unwrap_or_default();
    let (mime, data) = url
        .strip_prefix("data:")
        .and_then(|rest| rest.split_once(','))
        .and_then(|(head, data)| Some((head.strip_suffix(";base64")?, data)))
        .ok_or_else(|| {
            Fault::bad_request("Bedrock takes a part's media as bytes: a base64 data URL")
        })?;
    Ok(Part::Media(Media {
        of,
        mime,
        data,
        url,
    }))
}

/// One attempt's calls: what makes each, and the attempt's extensions,
/// which each carries.
pub struct Calls {
    connect: Connect,
    extensions: Extensions,
}

impl std::fmt::Debug for Calls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Calls").finish_non_exhaustive()
    }
}

/// A bearer key, over HTTP, beside its base URL.
impl Upstream for BedrockEmbeddings {
    fn name(&self) -> &str {
        NAME
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |extensions| Calls {
            connect: connect.clone(),
            extensions,
        })
    }
}

impl Format<Calls, Output, Input> for BedrockEmbeddings {
    fn name(&self) -> &str {
        NAME
    }

    /// One call per body of the request, each its own I/O with the
    /// attempt's extensions: sent as the sink closes, read in order as the
    /// answer opens.
    fn bind(&self, calls: Calls) -> Result<Connection<Output, Input>, Fault> {
        let (opened, opening) = oneshot::channel::<Vec<Answer>>();
        let incoming = async move {
            let answers = opening.await.map_err(|_| Fault::abandoned())?;
            let mut out = Vec::new();
            let mut index = 0;
            for answer in answers {
                answer.read(&mut out, &mut index).await?;
            }
            Ok(EventStream::stream(stream::iter(out.into_iter().map(Ok))))
        }
        .boxed();
        let outgoing = EventSink::preparing(move |events: &mut EventStream<Input>| {
            if let Some(fault) = events.refusal() {
                return Err(fault.clone());
            }
            if !events.is_single() {
                return Err(Fault::config(format!(
                    "{NAME} does not accept streaming inputs"
                )));
            }
            let none = || Fault::internal("a whole request has no input");
            let input = events.value().transpose()?.ok_or_else(none)?.clone();
            events.stop();
            let model = input.model.clone().unwrap_or_default();
            let path = format!("/model/{}/invoke", escape(&model));
            let mut sinks = Vec::new();
            let mut answers = Vec::new();
            for (at, body) in bodies(&input)?.into_iter().enumerate() {
                let io = (calls.connect)(calls.extensions.clone());
                let path = path.clone();
                let start = Start::new(Some(model.clone()));
                let body = Bytes::from(body.to_string());
                let (answer, mut sink) = bind_provider(
                    io,
                    NAME,
                    move |head, _| {
                        respan_gateway_http::path::set_path(head, &path)?;
                        let json = HeaderValue::from_static("application/json");
                        head.headers.insert(CONTENT_TYPE, json);
                        Ok(sends(body))
                    },
                    move |head, _| Some(read_answer(start, head)),
                );
                sink.prepare(&mut EventStream::single(input.clone()))?;
                sinks.push(sink);
                answers.push(Answer {
                    first: at == 0,
                    answer,
                });
            }
            // The receiver is gone only when the answer was dropped unread.
            let _ = opened.send(answers);
            Ok(Sent::new(sinks))
        });
        Ok((incoming, outgoing))
    }
}

/// Each call's body, in order: Cohere's one; the others' one per input.
fn bodies(input: &Input) -> Result<Vec<Value>, Fault> {
    let model = input.model.as_deref().unwrap_or_default();
    let family = Family::of(model).ok_or_else(|| {
        Fault::bad_request(format!(
            "a Bedrock embedding needs a Cohere Embed, Titan Embeddings, Nova or Marengo model, not {model:?}"
        ))
    })?;
    let input_type = input_type(input)?;
    let dimensions = input.dimensions;
    let no_dimensions = || Fault::bad_request(format!("Bedrock's {model} takes no `dimensions`"));
    let takes_dimensions = matches!(
        family,
        Family::Cohere { v4: true }
            | Family::TitanText { v2: true }
            | Family::TitanMultimodal
            | Family::Nova
    );
    if dimensions.is_some() && !takes_dimensions {
        return Err(no_dimensions());
    }
    if input_type.is_some() && !matches!(family, Family::Cohere { .. } | Family::Nova) {
        return Err(Fault::bad_request(
            "only Cohere and Nova take an `input_type`",
        ));
    }
    let items = (input.items.iter())
        .map(|item| match item {
            Item::Text(text) => Ok(vec![Part::Text(text)]),
            Item::Parts(parts) => parts.iter().map(part).collect(),
            _ => Err(Fault::bad_request(
                "Bedrock embeds texts and parts, not tokens",
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let reads = |sense: Sense| {
        let read = match family {
            Family::Cohere { .. } | Family::TitanMultimodal | Family::Marengo => {
                sense == Sense::Image
            }
            Family::Nova => true,
            Family::TitanText { .. } => false,
        };
        match read {
            true => Ok(()),
            false => Err(Fault::bad_request(format!(
                "Bedrock's {model} embeds no {}",
                sense.name()
            ))),
        }
    };
    for part in items.iter().flatten() {
        if let Part::Media(media) = part {
            reads(media.of)?;
        }
    }
    match family {
        Family::Cohere { v4 } => cohere(&items, v4, input_type, dimensions).map(|v| vec![v]),
        Family::TitanText { .. } => Ok(items
            .iter()
            .flatten()
            .filter_map(|part| match (part, dimensions) {
                (Part::Text(text), Some(dimensions)) => {
                    Some(json!({"inputText": text, "dimensions": dimensions}))
                }
                (Part::Text(text), None) => Some(json!({"inputText": text})),
                // Refused above: Titan Text reads no media.
                (Part::Media(_), _) => None,
            })
            .collect()),
        Family::TitanMultimodal => items
            .iter()
            .map(|parts| {
                let (text, image) = text_and_image(parts, "Titan Multimodal")?;
                let mut v = json!({});
                if let Some(text) = text {
                    v["inputText"] = text.into();
                }
                if let Some(image) = image {
                    v["inputImage"] = image.data.into();
                }
                if let Some(dimensions) = dimensions {
                    v["embeddingConfig"] = json!({"outputEmbeddingLength": dimensions});
                }
                Ok(v)
            })
            .collect(),
        Family::Nova => Ok(items
            .iter()
            .flatten()
            .map(|part| nova(part, input_type, dimensions))
            .collect()),
        Family::Marengo => items
            .iter()
            .map(|parts| {
                let source = |m: &Media| json!({"mediaSource": {"base64String": m.data}});
                Ok(match text_and_image(parts, "Marengo")? {
                    (Some(text), None) => json!({"inputType": "text", "text": {"inputText": text}}),
                    (None, Some(image)) => json!({"inputType": "image", "image": source(&image)}),
                    (Some(text), Some(image)) => {
                        let mut both = source(&image);
                        both["inputText"] = text.into();
                        json!({"inputType": "text_image", "text_image": both})
                    }
                    (None, None) => return Err(Fault::bad_request("an item needs a part")),
                })
            })
            .collect(),
    }
}

/// Cohere's one body: every text, or one item of parts (v3 one image, v4
/// texts and images as one input).
fn cohere(
    items: &[Vec<Part<'_>>],
    v4: bool,
    input_type: Option<&str>,
    dimensions: Option<u32>,
) -> Result<Value, Fault> {
    let texts: Option<Vec<&str>> = (items.iter())
        .map(|parts| match parts.as_slice() {
            [Part::Text(text)] => Some(*text),
            _ => None,
        })
        .collect();
    let mut v = match (texts, items) {
        (Some(texts), _) => json!({
            "texts": texts,
            "input_type": input_type.unwrap_or(SEARCH_DOCUMENT),
        }),
        (None, [parts]) if v4 => {
            let content = (parts.iter())
                .map(|part| match part {
                    Part::Text(text) => json!({"type": "text", "text": text}),
                    Part::Media(media) => json!({"type": "image_url", "image_url": media.url}),
                })
                .collect::<Vec<_>>();
            json!({
                "inputs": [{"content": content}],
                "input_type": input_type.unwrap_or(SEARCH_DOCUMENT),
            })
        }
        (None, [parts]) => match parts.as_slice() {
            [Part::Media(image)] => json!({"images": [image.url], "input_type": "image"}),
            _ => {
                return Err(Fault::bad_request(
                    "Cohere Embed v3 embeds texts, or one image alone",
                ));
            }
        },
        (None, _) => {
            return Err(Fault::bad_request(
                "Cohere embeds texts, or one item of parts",
            ));
        }
    };
    v["embedding_types"] = json!(["float"]);
    if let Some(dimensions) = dimensions {
        v["output_dimension"] = dimensions.into();
    }
    Ok(v)
}

/// The text and image of an item of at most one of each, as `model` takes.
fn text_and_image<'a>(
    parts: &[Part<'a>],
    model: &str,
) -> Result<(Option<&'a str>, Option<Media<'a>>), Fault> {
    let (mut text, mut image) = (None, None);
    for part in parts {
        let taken = match part {
            Part::Text(t) => text.replace(*t).is_some(),
            Part::Media(m) => image.replace(*m).is_some(),
        };
        if taken {
            return Err(Fault::bad_request(format!(
                "{model} embeds one text and one image at most in an item"
            )));
        }
    }
    Ok((text, image))
}

/// Nova's body of one part.
fn nova(part: &Part<'_>, purpose: Option<&str>, dimensions: Option<u32>) -> Value {
    let mut params = json!({"embeddingPurpose": purpose.unwrap_or(GENERIC_INDEX)});
    if let Some(dimensions) = dimensions {
        params["embeddingDimension"] = dimensions.into();
    }
    match part {
        Part::Text(text) => params["text"] = json!({"truncationMode": "END", "value": text}),
        Part::Media(media) => {
            let (key, format) = match media.of {
                Sense::Image => ("image", media_format(media.mime)),
                Sense::Audio => ("audio", media_format(media.mime)),
                Sense::Video => ("video", media_format(media.mime)),
            };
            let mut of = json!({"format": format, "source": {"bytes": media.data}});
            if media.of == Sense::Video {
                of["embeddingMode"] = "AUDIO_VIDEO_COMBINED".into();
            }
            params[key] = of;
        }
    }
    json!({"taskType": "SINGLE_EMBEDDING", "singleEmbeddingParams": params})
}

/// The format Nova names a media type by: its subtype, as Nova spells it.
fn media_format(mime: &str) -> &str {
    let subtype = mime.split_once('/').map_or(mime, |(_, s)| s);
    match subtype {
        "jpg" => "jpeg",
        "mpeg" if mime.starts_with("audio/") => "mp3",
        "x-wav" | "wave" | "vnd.wave" => "wav",
        "quicktime" => "mov",
        "x-matroska" => "mkv",
        "x-flv" => "flv",
        "x-ms-wmv" => "wmv",
        "3gpp" => "3gp",
        other => other,
    }
}

/// The client's `input_type`, an [`OpenAiEmbeddings`] field. Its
/// `encoding_format` is the client edge's; any other field is refused.
fn input_type(input: &Input) -> Result<Option<&str>, Fault> {
    let Some(native) = &input.native else {
        return Ok(None);
    };
    if &*native.format != OpenAiEmbeddings::NAME {
        return Err(Fault::bad_request(format!(
            "fields of format {} cannot be written in {NAME}",
            native.format
        )));
    }
    let mut input_type = None;
    for (key, value) in &native.fields {
        match (key.as_str(), value) {
            ("encoding_format", _) => {}
            ("input_type", Value::String(value)) => input_type = Some(value.as_str()),
            _ => {
                return Err(Fault::bad_request(format!(
                    "Bedrock embeddings take no `{key}` of this kind"
                )));
            }
        }
    }
    Ok(input_type)
}

/// One call's answer, and whether it is the request's first.
struct Answer {
    first: bool,
    answer: BoxFuture<'static, Result<EventStream<Output>, Fault>>,
}

impl Answer {
    /// Its events into `out`: its start only if the first call's, each
    /// vector numbered from `index` on.
    async fn read(self, out: &mut Vec<Output>, index: &mut u32) -> Result<(), Fault> {
        let mut events = self.answer.await?;
        while let Some(event) = events.next().await {
            match event? {
                Output::Start(_) if !self.first => {}
                Output::Embedding(mut embedding) => {
                    embedding.index = *index;
                    *index += 1;
                    out.push(Output::Embedding(embedding));
                }
                event => out.push(event),
            }
        }
        Ok(())
    }
}

/// The decoder of an answer: its vectors and its input tokens.
fn read_answer(start: Start, head: &ResponseHead) -> Box<dyn Decoder<Output>> {
    let counted = (head.headers.get(INPUT_TOKENS))
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    Box::new(Whole::new(move |raw: Bytes, out: &mut Vec<Output>| {
        decode(&start, counted, &raw, out)
    }))
}

/// The events of an answer: its vectors, Cohere's (`embeddings`, by type
/// or a list), Titan's (`embedding`), Nova's (`embeddings`, each an
/// `embedding`) or Marengo's (`data`, each an `embedding`); and the input
/// tokens it or its header counts.
fn decode(
    start: &Start,
    counted: Option<u64>,
    raw: &[u8],
    out: &mut Vec<Output>,
) -> Result<(), Fault> {
    let v: Value =
        serde_json::from_slice(raw).map_err(|_| Fault::upstream("Bedrock answered no JSON"))?;
    let each = |list: &Value| -> Option<Vec<Value>> {
        let list = list.as_array()?;
        let vector = |e: &Value| match e {
            Value::Array(_) => Some(e.clone()),
            _ => e.get("embedding").filter(|e| e.is_array()).cloned(),
        };
        list.iter().map(vector).collect()
    };
    let vectors = match (&v["embeddings"], &v["embedding"]) {
        (Value::Object(by_type), _) => by_type["float"].as_array().cloned(),
        (all @ Value::Array(_), _) => each(all),
        (_, one @ Value::Array(_)) => Some(vec![one.clone()]),
        _ => each(&v["data"]),
    };
    let vectors = vectors.ok_or_else(|| Fault::upstream("Bedrock answered no vectors"))?;
    out.push(Output::Start(start.clone()));
    for (index, vector) in vectors.iter().enumerate() {
        let numbers = vector.as_array().into_iter().flatten();
        let vector = numbers
            .map(|n| n.as_f64().map(|n| n as f32))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| Fault::upstream("a Bedrock vector is not numbers"))?;
        out.push(Output::Embedding(Embedding::new(index as u32, vector)));
    }
    let tokens = v["inputTextTokenCount"].as_u64().or(counted);
    out.extend(tokens.map(|tokens| Output::Usage(Usage::new(tokens, 0))));
    Ok(())
}

/// The sink of a request's calls: each sent as it closes, a few at a time.
struct Sent {
    sinks: Option<Vec<EventSink<Input>>>,
    closing: Option<BoxFuture<'static, Result<(), Fault>>>,
}

impl Sent {
    fn new(sinks: Vec<EventSink<Input>>) -> Self {
        Self {
            sinks: Some(sinks),
            closing: None,
        }
    }
}

/// Its input was encoded as it was prepared: nothing more is sent to it.
impl Sink<Result<Input, Fault>> for Sent {
    type Error = Fault;

    fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Fault>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, _: Result<Input, Fault>) -> Result<(), Fault> {
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Fault>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Fault>> {
        let this = self.get_mut();
        if let Some(sinks) = this.sinks.take() {
            let closes = stream::iter(sinks).map(|mut sink| async move { sink.close().await });
            this.closing = Some(closes.buffer_unordered(IN_FLIGHT).try_collect().boxed());
        }
        match &mut this.closing {
            Some(closing) => closing.poll_unpin(cx),
            None => Poll::Ready(Ok(())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_core::NativeFields;

    fn input(model: &str, items: Vec<Item>) -> Input {
        let mut input = Input::new(items);
        input.model = Some(model.to_owned());
        input
    }

    fn texts(model: &str, texts: &[&str]) -> Input {
        let items = texts.iter().map(|t| Item::Text((*t).to_owned())).collect();
        input(model, items)
    }

    fn parts(model: &str, parts: Value) -> Input {
        let parts = parts.as_array().cloned().unwrap_or_default();
        input(model, vec![Item::Parts(parts)])
    }

    const PNG: &str = "data:image/png;base64,iVBORw0KGgo=";

    #[test]
    fn cohere_embeds_every_text_in_one_call_and_titan_one_per_call() {
        let mut cohere = texts("us.cohere.embed-v4:0", &["a", "b"]);
        cohere.dimensions = Some(256);
        let bodies = bodies(&cohere).unwrap_or_default();
        assert_eq!(
            bodies,
            [json!({"texts": ["a", "b"], "input_type": "search_document",
                    "embedding_types": ["float"], "output_dimension": 256})]
        );
        let titan = bodies_of(&texts("amazon.titan-embed-text-v2:0", &["a", "b"]));
        assert_eq!(
            titan,
            [json!({"inputText": "a"}), json!({"inputText": "b"})]
        );
    }

    fn bodies_of(input: &Input) -> Vec<Value> {
        bodies(input).unwrap_or_else(|f| panic!("{}", f.message()))
    }

    #[test]
    fn an_item_of_parts_is_each_models_own_input() {
        let text_and_image =
            json!([{"type": "text", "text": "red"}, {"type": "image", "url": PNG}]);
        assert_eq!(
            bodies_of(&parts("cohere.embed-v4:0", text_and_image.clone())),
            [
                json!({"inputs": [{"content": [{"type": "text", "text": "red"},
                                            {"type": "image_url", "image_url": PNG}]}],
                    "input_type": "search_document", "embedding_types": ["float"]})
            ]
        );
        let image = json!([{"type": "image_url", "image_url": {"url": PNG}}]);
        assert_eq!(
            bodies_of(&parts("cohere.embed-english-v3", image)),
            [json!({"images": [PNG], "input_type": "image", "embedding_types": ["float"]})]
        );
        assert_eq!(
            bodies_of(&parts(
                "amazon.titan-embed-image-v1",
                text_and_image.clone()
            )),
            [json!({"inputText": "red", "inputImage": "iVBORw0KGgo="})]
        );
        assert_eq!(
            bodies_of(&parts(
                "twelvelabs.marengo-embed-3-0-v1:0",
                text_and_image.clone()
            )),
            [json!({"inputType": "text_image",
                    "text_image": {"inputText": "red", "mediaSource": {"base64String": "iVBORw0KGgo="}}})]
        );
        let mut nova = parts("amazon.nova-2-multimodal-embeddings-v1:0", text_and_image);
        nova.dimensions = Some(256);
        assert_eq!(
            bodies_of(&nova),
            [
                json!({"taskType": "SINGLE_EMBEDDING", "singleEmbeddingParams": {
                    "embeddingPurpose": "GENERIC_INDEX", "embeddingDimension": 256,
                    "text": {"truncationMode": "END", "value": "red"}}}),
                json!({"taskType": "SINGLE_EMBEDDING", "singleEmbeddingParams": {
                    "embeddingPurpose": "GENERIC_INDEX", "embeddingDimension": 256,
                    "image": {"format": "png", "source": {"bytes": "iVBORw0KGgo="}}}}),
            ]
        );
    }

    #[test]
    fn nova_takes_the_clients_input_type_as_its_purpose_and_reads_audio_and_video() {
        let mut nova = parts(
            "amazon.nova-2-multimodal-embeddings-v1:0",
            json!([{"type": "audio", "url": "data:audio/mpeg;base64,AA=="},
                   {"type": "video", "url": "data:video/quicktime;base64,AA=="}]),
        );
        let mut fields = serde_json::Map::new();
        fields.insert("input_type".into(), json!("GENERIC_RETRIEVAL"));
        nova.native = Some(NativeFields::new(OpenAiEmbeddings::NAME, fields));
        let bodies = bodies_of(&nova);
        let params = |i: usize| &bodies[i]["singleEmbeddingParams"];
        assert_eq!(params(0)["embeddingPurpose"], "GENERIC_RETRIEVAL");
        assert_eq!(params(0)["audio"]["format"], "mp3");
        assert_eq!(params(1)["video"]["format"], "mov");
        assert_eq!(params(1)["video"]["embeddingMode"], "AUDIO_VIDEO_COMBINED");
    }

    #[test]
    fn what_a_model_cannot_embed_is_refused_before_any_call() {
        let image_by_url = json!([{"type": "image", "url": "https://example.com/a.png"}]);
        let audio = json!([{"type": "audio", "url": "data:audio/wav;base64,AA=="}]);
        let two_images = json!([{"type": "image", "url": PNG}, {"type": "image", "url": PNG}]);
        let mut v3_dimensions = texts("cohere.embed-english-v3", &["a"]);
        v3_dimensions.dimensions = Some(256);
        for refused in [
            parts("cohere.embed-v4:0", image_by_url),
            parts("cohere.embed-v4:0", audio.clone()),
            parts("twelvelabs.marengo-embed-3-0-v1:0", audio),
            parts("amazon.titan-embed-image-v1", two_images),
            parts(
                "amazon.titan-embed-text-v2:0",
                json!([{"type": "image", "url": PNG}]),
            ),
            input(
                "amazon.titan-embed-text-v2:0",
                vec![Item::Tokens(vec![1, 2])],
            ),
            texts("twelvelabs.marengo-embed-2-7-v1:0", &["a"]),
            v3_dimensions,
        ] {
            let fault = bodies(&refused).err();
            assert!(
                fault.is_some_and(|f| f.is(respan_gateway_core::FaultKind::Client)),
                "{refused:?}"
            );
        }
    }

    #[test]
    fn every_familys_answer_is_its_vectors_and_counted_tokens() {
        let start = Start::new(Some("m".to_owned()));
        let read = |answer: Value, counted: Option<u64>| {
            let mut out = Vec::new();
            decode(&start, counted, answer.to_string().as_bytes(), &mut out).map(|()| out)
        };
        let vectors = |out: &[Output]| -> Vec<Vec<f32>> {
            (out.iter())
                .filter_map(|e| match e {
                    Output::Embedding(e) => Some(e.vector.clone()),
                    _ => None,
                })
                .collect()
        };
        let tokens = |out: &[Output]| {
            out.iter().find_map(|e| match e {
                Output::Usage(u) => Some(u.input_tokens),
                _ => None,
            })
        };
        for (answer, counted, want, counts) in [
            (
                json!({"embeddings": {"float": [[1.0], [2.0]]}, "response_type": "embeddings_by_type"}),
                Some(2),
                vec![vec![1.0], vec![2.0]],
                Some(2),
            ),
            (
                json!({"embedding": [1.0, 2.0], "inputTextTokenCount": 3}),
                Some(9),
                vec![vec![1.0, 2.0]],
                Some(3),
            ),
            (
                json!({"embeddings": [{"embeddingType": "IMAGE", "embedding": [0.5]}]}),
                Some(0),
                vec![vec![0.5]],
                Some(0),
            ),
            (
                json!({"data": [{"embedding": [0.25]}]}),
                None,
                vec![vec![0.25]],
                None,
            ),
        ] {
            let out = read(answer.clone(), counted).unwrap_or_default();
            assert_eq!(vectors(&out), want, "{answer}");
            assert_eq!(tokens(&out), counts, "{answer}");
        }
        assert!(read(json!({"message": "x"}), None).is_err());
    }
}
