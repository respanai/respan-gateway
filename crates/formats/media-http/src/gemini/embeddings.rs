//! Gemini's embedding models through `batchEmbedContents`: a provider format
//! of the [`embedding`](respan_gateway_media::embedding) kind,
//! [`GeminiEmbeddings`].

use super::is_segment;
use crate::edge::{bind_provider, sends};
use crate::openai::{OpenAiEmbeddings, usage_extras};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use respan_gateway_conversation::Usage;
use respan_gateway_conversation_http::fields::{base64_bytes, data_url, invalid};
use respan_gateway_core::{Connection, Fault, Format, Native, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::header::{CONTENT_TYPE, HeaderValue};
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::{
    Connect, Decoder, Encoder, KeyIn, ProviderIo, Upstream, Whole, native::Json,
};
use respan_gateway_media::embedding::{Embedding, Input, Item, Output, Start};
use serde_json::{Map, Value, json};

const NAME: &str = "gemini-embeddings";

/// How an OpenAI-style client's parts arrive: as that format's natives.
type Theirs = crate::Own<OpenAiEmbeddings>;
/// The method a model's texts are embedded by, all in one call.
const BATCH: &str = ":batchEmbedContents";

/// Gemini's embedding models as the Gemini API serves them, as a provider
/// only: a `POST` to `.../models/{model}:batchEmbedContents`, one request
/// per text, each text one part.
///
/// An item given as parts (`{"type": "text" | "image_url" | "input_video" |
/// "input_audio" | "input_file", ..}`, one item as `{"content": [..]}`, or
/// a bare list of parts) is one request of those parts, one joint vector.
/// A part's `data:` URL, or raw base64 with its `format` or `mime_type`, is
/// sent inline; an `https` URL is sent for Gemini to fetch, its type the
/// part's `format`, `mime_type` or the URL's extension. Any other source is
/// refused, 400, before the call.
///
/// `dimensions` is each request's `outputDimensionality`. An answer's
/// vectors are its `embeddings`' `values`, in order; its usage is
/// `usageMetadata`'s `promptTokenCount`, which Gemini reports for some
/// models only (not `gemini-embedding-001`), and which the OpenAI-compatible
/// endpoint reports for none. Its `promptTokenDetails` are the usage's
/// `prompt_tokens_details`, a `{modality}_tokens` each, to a client that
/// writes them.
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
        let contents = match item {
            Item::Text(text) => vec![vec![json!({ "text": text })]],
            Item::Native(n) => contents_of(n.value::<Theirs>().ok_or_else(|| {
                invalid("input", "must be texts or parts of an OpenAI-style client")
            })?)?,
            _ => return Err(invalid("input", "must be texts or parts for Gemini")),
        };
        for parts in contents {
            let mut request = json!({
                "model": format!("models/{model}"),
                "content": { "parts": parts },
            });
            if let Some(n) = input.dimensions {
                request["outputDimensionality"] = n.into();
            }
            requests.push(request);
        }
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

/// The contents a client's parts item holds, each the parts of one request:
/// a bare list of parts is one, else every element is one, a text or an
/// object of `content`.
fn contents_of(v: &Value) -> Result<Vec<Vec<Value>>, Fault> {
    let bad = || invalid("input", "must be texts or lists of parts");
    let items = v.as_array().filter(|a| !a.is_empty()).ok_or_else(bad)?;
    let bare = |i: &Value| i.get("type").is_some() && i.get("content").is_none();
    if items.iter().all(bare) {
        return Ok(vec![items.iter().map(part).collect::<Result<_, _>>()?]);
    }
    let content = |item: &Value| match (item, &item["content"]) {
        (Value::String(text), _) | (_, Value::String(text)) => Ok(vec![json!({ "text": text })]),
        (_, Value::Array(parts)) if !parts.is_empty() => parts.iter().map(part).collect(),
        _ => Err(bad()),
    };
    items.iter().map(content).collect()
}

/// A client's part as a Gemini part: a text, or media sent inline or by URL.
fn part(p: &Value) -> Result<Value, Fault> {
    let kind = p["type"].as_str().unwrap_or_default();
    let (family, inner) = match kind {
        "text" => {
            let text = p["text"].as_str();
            return text
                .map(|text| json!({ "text": text }))
                .ok_or_else(|| invalid("input", "has a text part with no text"));
        }
        "image_url" | "image" => ("image", &p[kind]),
        "input_video" | "video_url" | "video" => ("video", &p[kind]),
        "input_audio" | "audio" => ("audio", &p[kind]),
        "input_file" | "file" => ("application", &p[kind]),
        _ => return Err(invalid("input", "has a part of a type Gemini cannot embed")),
    };
    let fields = if inner.is_object() { inner } else { p };
    let source = inner.as_str().or_else(|| {
        let keys = ["url", "data", "file_data", "file_url"];
        keys.iter().find_map(|k| fields[k].as_str())
    });
    let source = source.ok_or_else(|| invalid("input", "has a media part with no data"))?;
    let named = ["mime_type", "format"]
        .iter()
        .find_map(|k| fields[k].as_str());
    let named = named.map(|n| match n.contains('/') {
        true => n.to_owned(),
        false => format!("{family}/{n}"),
    });
    let inline =
        |mime: String, data: String| json!({ "inline_data": { "mime_type": mime, "data": data } });
    if let Some((mime, bytes)) = data_url(source).map_err(|e| invalid("input", e.reason()))? {
        let mime = named.filter(|_| mime == "text/plain").unwrap_or(mime);
        return Ok(inline(mime, STANDARD.encode(bytes)));
    }
    if source.starts_with("https://") {
        let mime = named.or_else(|| mime_of_url(source));
        let mime = mime.ok_or_else(|| invalid("input", "has a URL of no known media type"))?;
        return Ok(json!({ "file_data": { "mime_type": mime, "file_uri": source } }));
    }
    match (named, base64_bytes(source)) {
        (Some(mime), Some(_)) => Ok(inline(mime, source.to_owned())),
        _ => Err(invalid(
            "input",
            "must be a `data:` URL, an https URL, or base64 with its format",
        )),
    }
}

/// The media type a URL's file extension names.
fn mime_of_url(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next()?;
    let mime = match path.rsplit_once('.')?.1.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mp3" => "audio/mp3",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        _ => return None,
    };
    Some(mime.to_owned())
}

/// `usageMetadata`'s `promptTokenDetails` as `prompt_tokens_details`: the
/// tokens of each modality, summed, as `{modality}_tokens`.
fn modality_tokens(details: &Value) -> Option<Native> {
    let mut by = Map::new();
    for d in details.as_array().into_iter().flatten() {
        let (Some(modality), Some(n)) = (d["modality"].as_str(), d["tokenCount"].as_u64()) else {
            continue;
        };
        let key = format!("{}_tokens", modality.to_ascii_lowercase());
        let total = by.get(&key).and_then(Value::as_u64).unwrap_or(0);
        by.insert(key, total.saturating_add(n).into());
    }
    if by.is_empty() {
        return None;
    }
    let extras = Map::from_iter([("prompt_tokens_details".to_owned(), Value::Object(by))]);
    usage_extras::<Theirs>(extras)
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
        let details = &v["usageMetadata"]["promptTokenDetails"];
        out.extend(modality_tokens(details).map(Output::Native));
        Ok(())
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_http::http;

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
    fn a_client_s_parts_are_one_request_of_parts() {
        let asked = json!([{ "content": [
            { "type": "text", "text": "a" },
            { "type": "input_video", "input_video": { "data": "data:video/mp4;base64,AAAA", "format": "mp4" } },
            { "type": "image_url", "image_url": { "url": "https://x.test/i.png" } },
        ] }]);
        let item = Item::Native(Native::json::<Theirs>(asked));
        let (_, body) = call(&[], |i| i.items = vec![item]).expect("a call");
        let parts = json!([
            { "text": "a" },
            { "inline_data": { "mime_type": "video/mp4", "data": "AAAA" } },
            { "file_data": { "mime_type": "image/png", "file_uri": "https://x.test/i.png" } },
        ]);
        assert_eq!(body["requests"][0]["content"]["parts"], parts);
        let url = json!([{ "type": "image_url", "image_url": { "url": "http://x.test/i.png" } }]);
        let item = Item::Native(Native::json::<Theirs>(url));
        assert!(call(&[], |i| i.items = vec![item]).is_err());
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
        let Some(Output::Native(n)) = video.last() else {
            panic!("no details: {video:?}");
        };
        assert_eq!(
            n.value::<Theirs>(),
            Some(&json!({ "usage": { "prompt_tokens_details":
                { "video_tokens": 1056, "text_tokens": 3 } } }))
        );
        let uncounted = read(json!({ "embeddings": [{ "values": [0.5] }] }));
        assert_eq!(uncounted.len(), 2);
    }
}
