//! Gemini's embedding models through `batchEmbedContents`: a provider format
//! of the [`embedding`](respan_gateway_media::embedding) kind,
//! [`GeminiEmbeddings`].

use super::is_segment;
use crate::edge::{bind_provider, sends};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use respan_gateway_conversation::MediaSource;
use respan_gateway_conversation::Usage;
use respan_gateway_conversation_http::fields::invalid;
use respan_gateway_core::{Connection, Fault, Format, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::header::{CONTENT_TYPE, HeaderValue};
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::{Connect, Decoder, Encoder, KeyIn, ProviderIo, Upstream, Whole};
use respan_gateway_media::embedding::{Embedding, Input, Item, Modality, Output, Part, Start};
use serde_json::{Value, json};

const NAME: &str = "gemini-embeddings";
/// The method a model's texts are embedded by, all in one call.
const BATCH: &str = ":batchEmbedContents";

/// Gemini's embedding models as the Gemini API serves them, as a provider
/// only: a `POST` to `.../models/{model}:batchEmbedContents`, one request
/// per text, each text one part.
///
/// An item of parts is one request of those parts, one joint vector: a
/// text as `text`, media as `inline_data` from its bytes, or as `file_data`
/// from an `https` URL, for Gemini to fetch. Any other URL is refused, 400,
/// before the call.
///
/// `dimensions` is each request's `outputDimensionality`. An answer's
/// vectors are its `embeddings`' `values`, in order; its usage is
/// `usageMetadata`'s `promptTokenCount`, which Gemini reports for some
/// models only (not `gemini-embedding-001`), and which the OpenAI-compatible
/// endpoint reports for none. Its `promptTokenDetails` are the input tokens
/// of each [`Modality`].
///
/// Token ids are refused, 400, before the call; fields of another format,
/// such as an end user's id, are dropped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GeminiEmbeddings;

impl GeminiEmbeddings {
    /// The format's name, `gemini-embeddings`.
    pub const NAME: &'static str = NAME;
}

/// Its spec's: the key in `x-goog-api-key`, over HTTP.
impl Upstream for GeminiEmbeddings {
    fn name(&self) -> &str {
        NAME
    }

    fn key(&self) -> KeyIn {
        KeyIn::Header("x-goog-api-key")
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for GeminiEmbeddings {
    fn name(&self) -> &str {
        NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        Ok(bind_provider(io, NAME, post, |_, call| {
            Some(read_response(model_of(call.uri().path())))
        }))
    }
}

/// The call of `input`: its model's `batchEmbedContents`, a request a text.
fn post(head: &mut RequestHead, input: &Input) -> Result<Box<dyn Encoder<Input>>, Fault> {
    let model = input.model.as_deref().unwrap_or_default();
    let model = model.strip_prefix("models/").unwrap_or(model);
    if !is_segment(model) {
        return Err(invalid(
            "model",
            "is needed by Gemini, such as gemini-embedding-001",
        ));
    }
    let mut requests = Vec::new();
    for item in &input.items {
        let parts = match item {
            Item::Text(text) => vec![json!({ "text": text })],
            Item::Parts(p) => p.parts.iter().map(part).collect::<Result<Vec<_>, _>>()?,
            _ => return Err(invalid("input", "must be texts or parts for Gemini")),
        };
        let mut request = json!({
            "model": format!("models/{model}"),
            "content": { "parts": parts },
        });
        if let Some(n) = input.dimensions {
            request["outputDimensionality"] = n.into();
        }
        requests.push(request);
    }
    head.uri = format!("/models/{model}{BATCH}")
        .parse()
        .map_err(|e| Fault::internal("a call's path is no path").with_source(e))?;
    let json = HeaderValue::from_static("application/json");
    head.headers.insert(CONTENT_TYPE, json);
    let body = serde_json::to_vec(&json!({ "requests": requests }))
        .map_err(|e| Fault::internal("a body could not be written").with_source(e))?;
    Ok(sends(Bytes::from(body)))
}

/// A part as a Gemini part.
fn part(part: &Part) -> Result<Value, Fault> {
    let media = match part {
        Part::Text(text) => return Ok(json!({ "text": text })),
        Part::Media(media) => media,
        _ => return Err(invalid("input", "has a part Gemini cannot embed")),
    };
    let mime = &media.mime_type;
    match &media.source {
        MediaSource::Bytes(bytes) => {
            let data = STANDARD.encode(bytes);
            Ok(json!({ "inline_data": { "mime_type": mime, "data": data } }))
        }
        MediaSource::Url(url) if url.starts_with("https://") => {
            Ok(json!({ "file_data": { "mime_type": mime, "file_uri": url } }))
        }
        _ => Err(invalid("input", "must be bytes or an https URL")),
    }
}

/// The modality Gemini names, if the kind has it.
fn modality(name: &str) -> Option<Modality> {
    Some(match name {
        "TEXT" => Modality::Text,
        "IMAGE" => Modality::Image,
        "AUDIO" => Modality::Audio,
        "VIDEO" => Modality::Video,
        "DOCUMENT" => Modality::Document,
        _ => return None,
    })
}

/// The model a call at `path`, `.../models/{model}:batchEmbedContents`, is of.
fn model_of(path: &str) -> Option<String> {
    let model = path.strip_suffix(BATCH)?.rsplit('/').next()?;
    Some(model.to_owned())
}

/// The decoder of an answer to a call of `model`: its vectors, then the
/// tokens Gemini counts.
fn read_response(model: Option<String>) -> Box<dyn Decoder<Output>> {
    Box::new(Whole::new(move |raw: Bytes, out: &mut Vec<Output>| {
        let v: Value = serde_json::from_slice(&raw)
            .map_err(|_| Fault::upstream("Gemini's answer is not JSON"))?;
        let Some(embeddings) = v["embeddings"].as_array().filter(|e| !e.is_empty()) else {
            return Err(Fault::upstream("Gemini's answer holds no vector"));
        };
        out.push(Output::Start(Start::new(model.clone())));
        for (at, embedding) in embeddings.iter().enumerate() {
            let numbers = embedding["values"].as_array().into_iter().flatten();
            let vector = numbers
                .map(|n| n.as_f64().map(|n| n as f32))
                .collect::<Option<_>>();
            let vector = vector.ok_or_else(|| Fault::upstream("a vector is not numbers"))?;
            let at = u32::try_from(at).map_err(|_| Fault::upstream("too many vectors"))?;
            out.push(Output::Embedding(Embedding::new(at, vector)));
        }
        let counted = v["usageMetadata"]["promptTokenCount"].as_u64();
        out.extend(counted.map(|n| Output::Usage(Usage::new(n, 0))));
        let details = v["usageMetadata"]["promptTokenDetails"].as_array();
        for d in details.into_iter().flatten() {
            let kind = d["modality"].as_str().and_then(modality);
            if let (Some(kind), Some(n)) = (kind, d["tokenCount"].as_u64()) {
                out.push(Output::InputTokens(kind, n));
            }
        }
        Ok(())
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_conversation::Media;
    use respan_gateway_core::Native;
    use respan_gateway_http::http;
    use respan_gateway_media::embedding::Parts;

    fn call(items: &[&str], f: impl FnOnce(&mut Input)) -> Result<(String, Value), Fault> {
        let items = items.iter().map(|t| Item::Text((*t).into()));
        let mut input = Input::new(items.collect::<Vec<_>>());
        input.model = Some("gemini-embedding-001".into());
        f(&mut input);
        let mut head = http::Request::new(()).into_parts().0;
        let mut encoder = post(&mut head, &input)?;
        let mut body = Vec::new();
        encoder.encode(&input, &mut body)?;
        Ok((
            head.uri.path().to_owned(),
            serde_json::from_slice(&body[0]).expect("JSON"),
        ))
    }

    #[test]
    fn every_text_is_one_request_of_one_batch() {
        let (path, body) = call(&["a", "b"], |i| i.dimensions = Some(768)).expect("a call");
        assert_eq!(path, "/models/gemini-embedding-001:batchEmbedContents");
        let request = |text: &str| {
            json!({ "model": "models/gemini-embedding-001",
                    "content": { "parts": [{ "text": text }] },
                    "outputDimensionality": 768 })
        };
        assert_eq!(body, json!({ "requests": [request("a"), request("b")] }));
        let parts = |i: &mut Input| i.items = vec![Item::Native(Native::new(json!([{}])))];
        assert!(call(&["a"], parts).is_err());
        assert!(call(&["a"], |i| i.model = None).is_err());
    }

    #[test]
    fn parts_are_one_request_of_parts() {
        let media = |mime: &str, source| Part::Media(Media::new(mime, source));
        let bytes = MediaSource::Bytes(Bytes::from_static(b"abc"));
        let url = |u: &str| MediaSource::Url(u.into());
        let parts = vec![
            Part::Text("a".into()),
            media("video/mp4", bytes),
            media("image/png", url("https://x.test/i.png")),
        ];
        let (_, body) =
            call(&[], |i| i.items = vec![Item::Parts(Parts::new(parts))]).expect("a call");
        let sent = json!([
            { "text": "a" },
            { "inline_data": { "mime_type": "video/mp4", "data": "YWJj" } },
            { "file_data": { "mime_type": "image/png", "file_uri": "https://x.test/i.png" } },
        ]);
        assert_eq!(body["requests"][0]["content"]["parts"], sent);
        let http = vec![media("image/png", url("http://x.test/i.png"))];
        assert!(call(&[], |i| i.items = vec![Item::Parts(Parts::new(http))]).is_err());
    }

    #[test]
    fn an_answer_is_its_vectors_and_the_tokens_gemini_counts() {
        let read = |answer: Value| {
            let mut decoder = read_response(model_of("/v1beta/models/m:batchEmbedContents"));
            let mut out = Vec::new();
            decoder
                .decode(answer.to_string().into(), &mut out)
                .expect("read");
            decoder.end(Ok(()), &mut out).expect("ended");
            out
        };
        let counted = read(json!({
            "embeddings": [{ "values": [0.5, 1.0] }, { "values": [2.0] }],
            "usageMetadata": { "promptTokenCount": 3 },
        }));
        assert_eq!(
            counted,
            [
                Output::Start(Start::new(Some("m".into()))),
                Output::Embedding(Embedding::new(0, vec![0.5, 1.0])),
                Output::Embedding(Embedding::new(1, vec![2.0])),
                Output::Usage(Usage::new(3, 0)),
            ]
        );
        let video = read(json!({
            "embeddings": [{ "values": [0.5] }],
            "usageMetadata": { "promptTokenCount": 1059, "promptTokenDetails": [
                { "modality": "VIDEO", "tokenCount": 1056 },
                { "modality": "TEXT", "tokenCount": 3 }] },
        }));
        assert_eq!(
            video[video.len() - 2..],
            [
                Output::InputTokens(Modality::Video, 1056),
                Output::InputTokens(Modality::Text, 3),
            ]
        );
        let uncounted = read(json!({ "embeddings": [{ "values": [0.5] }] }));
        assert_eq!(uncounted.len(), 2);
    }
}
