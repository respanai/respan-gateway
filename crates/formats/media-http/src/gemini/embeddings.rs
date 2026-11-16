//! Gemini's embedding models through `embedContent` and
//! `batchEmbedContents`: a format of the
//! [`embedding`](respan_gateway_media::embedding) kind, [`GeminiEmbeddings`].

use super::{is_segment, refusal};
use crate::edge::{bind_provider, floats, sends};
use respan_gateway_conversation::{Media, MediaSource, Usage};
use respan_gateway_conversation_http::fields::{base64_bytes, invalid, merge_missing};
use respan_gateway_conversation_http::gemini::{media_part, method_of};
use respan_gateway_core::{Connection, Fault, Format, Native, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::header::{CONTENT_TYPE, HeaderValue};
use respan_gateway_http::http::{
    self, Method, Uri, request::Parts as RequestHead, response::Parts as ResponseHead,
};
use respan_gateway_http::{All, ClientIo, Decoder, Encoder, ProviderIo, Whole, native::Json};
use respan_gateway_http::{Connect, KeyIn, Upstream};
use respan_gateway_media::embedding::{
    Embedding, Input, Item, Modality, Output, Part, Parts, Start,
};
use serde_json::{Map, Value, json};

const NAME: &str = "gemini-embeddings";

type Key = crate::Own<GeminiEmbeddings>;

/// The method of one request, answered one vector.
const ONE: &str = "embedContent";
/// The method of several requests, answered a vector each.
const BATCH: &str = "batchEmbedContents";

/// A request's size of vector, by its name in JSON and in the API's own.
const SIZE: [&str; 2] = ["outputDimensionality", "output_dimensionality"];

/// The modalities a `promptTokenDetails` counts, by Gemini's names.
const MODALITIES: [(&str, Modality); 5] = [
    ("TEXT", Modality::Text),
    ("IMAGE", Modality::Image),
    ("AUDIO", Modality::Audio),
    ("VIDEO", Modality::Video),
    ("DOCUMENT", Modality::Document),
];

/// Gemini's embedding models as the Gemini API serves them: a `POST` to
/// `.../models/{model}:batchEmbedContents`, one request per item.
///
/// A text is a request of one `text` part. An item of parts is a request
/// of those parts, one joint vector: a text as `text`, media as
/// `inlineData` from its bytes, or as `fileData` from its URL, for Gemini
/// to fetch. `dimensions` is each request's `outputDimensionality`, and an
/// item's native fields, such as `taskType`, go in its request.
///
/// An answer's vectors are its `embeddings`' `values`, in order; its usage
/// is `usageMetadata`'s `promptTokenCount`, which Gemini reports for some
/// models only (not `gemini-embedding-001`), and which the OpenAI-compatible
/// endpoint reports for none. Its `promptTokenDetails` are the input tokens
/// of each [`Modality`].
///
/// Token ids are refused, 400, before the call; fields of another format,
/// such as an end user's id, are dropped.
///
/// A client speaks it too, of any provider of the kind: a `POST` to
/// `.../models/{model}:embedContent`, one request, or `:batchEmbedContents`,
/// its `requests`. A request's `content` is an item: a text if it is one
/// `text` part and says no more, else parts, of `text`, `inlineData` and
/// `fileData`. An `outputDimensionality` every request says is the
/// `dimensions`; a request's other fields, such as `taskType` and `title`,
/// and sizes that differ, are its item's native fields, which only a Gemini
/// provider is sent. The answer is `embedding`, or
/// `embeddings`, by index, and the usage as `usageMetadata`.
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
            let model = call_of(call.uri()).map(|(model, _)| model);
            Some(read_response(model))
        }))
    }
}

/// The model, unescaped, of a call at `uri`, and whether it is a batch:
/// `.../models/{model}:embedContent` or `:batchEmbedContents`.
fn call_of(uri: &Uri) -> Option<(String, bool)> {
    let (model, method) = method_of(uri)?;
    match method {
        ONE => Some((model, false)),
        BATCH => Some((model, true)),
        _ => None,
    }
}

/// The call of `input`: its model's `batchEmbedContents`, a request an item.
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
        let (parts, native) = match item {
            Item::Text(text) => (vec![json!({ "text": text })], None),
            Item::Parts(p) => {
                let parts = p.parts.iter().map(part_json);
                (parts.collect::<Result<Vec<_>, _>>()?, Some(&p.native))
            }
            _ => return Err(invalid("input", "must be texts or parts for Gemini")),
        };
        let mut request = Map::new();
        request.insert("model".into(), format!("models/{model}").into());
        request.insert("content".into(), json!({ "parts": parts }));
        if let Some(n) = input.dimensions {
            request.insert("outputDimensionality".into(), n.into());
        }
        if let Some(native) = native {
            merge_missing(&mut request, native.fields::<Key>());
        }
        requests.push(Value::Object(request));
    }
    head.uri = format!("/models/{model}:{BATCH}")
        .parse()
        .map_err(|e| Fault::internal("a call's path is no path").with_source(e))?;
    let json = HeaderValue::from_static("application/json");
    head.headers.insert(CONTENT_TYPE, json);
    let body = serde_json::to_vec(&json!({ "requests": requests }))
        .map_err(|e| Fault::internal("a body could not be written").with_source(e))?;
    Ok(sends(Bytes::from(body)))
}

/// A part as a Gemini part.
fn part_json(part: &Part) -> Result<Value, Fault> {
    match part {
        Part::Text(text) => Ok(json!({ "text": text })),
        Part::Media(media) => Ok(media_part(media)),
        _ => Err(invalid("input", "has a part Gemini cannot embed")),
    }
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
            let named = d["modality"].as_str();
            let kind = MODALITIES.iter().find(|(name, _)| Some(*name) == named);
            if let (Some((_, kind)), Some(n)) = (kind, d["tokenCount"].as_u64()) {
                out.push(Output::InputTokens(*kind, n));
            }
        }
        Ok(())
    }))
}

// --- A client of Gemini's embedding models: its requests read, the kind's
// answers written.

impl Format<ClientIo, Input, Output> for GeminiEmbeddings {
    fn name(&self) -> &str {
        NAME
    }

    fn matches(&self, io: &ClientIo) -> bool {
        let request = io.request();
        request.method() == Method::POST && call_of(request.uri()).is_some()
    }

    fn bind(&self, io: ClientIo) -> Result<Connection<Input, Output>, Fault> {
        Ok(io.with_error(refusal).bind(
            move |head| {
                let head = head.clone();
                Box::new(Whole::new(move |raw: Bytes, out: &mut Vec<Input>| {
                    out.push(read_request(&head, &raw)?);
                    Ok(())
                }))
            },
            move |reply| {
                let mut head = reply.head();
                let encoder = response_encoder(reply.request(), &mut head);
                Ok(reply.write(head, encoder))
            },
        ))
    }
}

/// A field of `v` by its name in JSON, `camel`, or in the API's own, `snake`:
/// Gemini reads both.
fn field<'a>(v: &'a Value, camel: &str, snake: &str) -> &'a Value {
    match &v[camel] {
        Value::Null => &v[snake],
        said => said,
    }
}

/// A client's call: its model, and an item of each of its requests.
fn read_request(head: &RequestHead, raw: &[u8]) -> Result<Input, Fault> {
    let (model, batch) = call_of(&head.uri)
        .ok_or_else(|| Fault::bad_request("the request is to no embedContent endpoint"))?;
    let mut v: Map<String, Value> = serde_json::from_slice(raw)
        .map_err(|_| Fault::bad_request("the request body is not a JSON object"))?;
    let mut requests = match batch {
        false => vec![v],
        true => match v.remove("requests") {
            Some(Value::Array(all)) if !all.is_empty() => {
                let object = |(i, request)| match request {
                    Value::Object(request) => Ok(request),
                    _ => Err(invalid(&format!("requests[{i}]"), "must be an object")),
                };
                (all.into_iter().enumerate().map(object)).collect::<Result<Vec<_>, _>>()?
            }
            _ => return Err(invalid("requests", "must be a list of requests")),
        },
    };
    // A size the kind can say, as every request says it, goes as one.
    let size = |request: &Map<String, Value>| {
        let said = SIZE.iter().find_map(|key| request.get(*key));
        said.and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
    };
    let dimensions = requests.first().and_then(size);
    let dimensions = dimensions.filter(|n| requests.iter().all(|r| size(r) == Some(*n)));
    let mut items = Vec::with_capacity(requests.len());
    for (i, request) in requests.iter_mut().enumerate() {
        request.remove("model");
        if dimensions.is_some() {
            SIZE.iter().for_each(|key| drop(request.remove(*key)));
        }
        let at = match batch {
            true => format!("requests[{i}]."),
            false => String::new(),
        };
        items.push(read_item(request, &at)?);
    }
    let mut input = Input::new(items);
    input.model = Some(model);
    input.dimensions = dimensions;
    Ok(input)
}

/// The item of a client's `request`, named by `at` in a refusal: a text if
/// its `content` is one `text` part and it says no more, else parts, the
/// rest of it their native fields.
fn read_item(request: &mut Map<String, Value>, at: &str) -> Result<Item, Fault> {
    let content = request.remove("content").unwrap_or_default();
    let mut parts = Vec::new();
    for (j, part) in content["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let unread = |why| invalid(&format!("{at}content.parts[{j}]"), why);
        parts.push(read_part(part).map_err(unread)?);
    }
    Ok(match (&mut parts[..], request.is_empty()) {
        ([], _) => return Err(invalid(&format!("{at}content"), "must have parts")),
        ([Part::Text(text)], true) => Item::Text(std::mem::take(text)),
        _ => {
            let mut item = Parts::new(parts);
            item.native = Native::json::<Key>(std::mem::take(request));
            Item::Parts(item)
        }
    })
}

/// A client's part, if it is one the kind has, else why it is not: a text,
/// or media, its bytes inline or a file's URL. A file that names no type is
/// of any.
fn read_part(part: &Value) -> Result<Part, &'static str> {
    if let Some(text) = part["text"].as_str() {
        return Ok(Part::Text(text.to_owned()));
    }
    let mime = |data: &Value| {
        field(data, "mimeType", "mime_type")
            .as_str()
            .map(str::to_owned)
    };
    let (inline, file) = (
        field(part, "inlineData", "inline_data"),
        field(part, "fileData", "file_data"),
    );
    let media = match (inline, file) {
        (Value::Null, Value::Null) => return Err("is neither text nor media"),
        (Value::Null, file) => {
            let url = field(file, "fileUri", "file_uri").as_str();
            let url = url.ok_or("must have a `fileUri`")?;
            let mime = mime(file).unwrap_or_else(|| "*/*".to_owned());
            Media::new(mime, MediaSource::Url(url.to_owned()))
        }
        (inline, _) => {
            let bytes = inline["data"].as_str().and_then(base64_bytes);
            let (Some(mime), Some(bytes)) = (mime(inline), bytes) else {
                return Err("must have a `mimeType` and base64 `data`");
            };
            Media::new(mime, MediaSource::Bytes(bytes.into()))
        }
    };
    Ok(Part::Media(media))
}

/// The encoder of an answer to `request`: its vector, or the vectors of a
/// batch, and the usage.
fn response_encoder(
    request: &http::Request<Bytes>,
    head: &mut ResponseHead,
) -> Box<dyn Encoder<Output>> {
    let batch = call_of(request.uri()).is_some_and(|(_, batch)| batch);
    let json = HeaderValue::from_static("application/json");
    head.headers.insert(CONTENT_TYPE, json);
    Box::new(All::new(move |events: &[Output]| {
        serde_json::to_string(&response_of(events, batch))
            .map_err(|e| Fault::internal("a body could not be written").with_source(e))
    }))
}

/// A response of `events`: their vectors by index, as `embeddings` or, of
/// one request, the first as `embedding`, and their usage.
fn response_of(events: &[Output], batch: bool) -> Value {
    let (mut vectors, mut usage, mut details) = (Vec::new(), None, Vec::<(&str, u64)>::new());
    for event in events {
        match event {
            Output::Embedding(e) => vectors.push((e.index, json!({ "values": floats(&e.vector) }))),
            Output::Usage(u) => *usage.get_or_insert_with(Usage::default) += u,
            Output::InputTokens(modality, n) => {
                // A modality Gemini has no name for is left out.
                let Some((name, _)) = MODALITIES.iter().find(|(_, m)| m == modality) else {
                    continue;
                };
                match details.iter_mut().find(|(seen, _)| seen == name) {
                    Some((_, count)) => *count = count.saturating_add(*n),
                    None => details.push((name, *n)),
                }
            }
            _ => {}
        }
    }
    vectors.sort_by_key(|(index, _)| *index);
    let mut vectors = vectors.into_iter().map(|(_, vector)| vector);
    let mut response = match batch {
        true => json!({ "embeddings": vectors.collect::<Vec<_>>() }),
        false => json!({ "embedding": vectors.next() }),
    };
    if usage.is_some() || !details.is_empty() {
        let mut counted = Map::new();
        if let Some(usage) = usage {
            counted.insert("promptTokenCount".into(), usage.input_tokens.into());
        }
        if !details.is_empty() {
            let detail = |(name, n): &(&str, u64)| json!({ "modality": name, "tokenCount": n });
            let details = details.iter().map(detail).collect::<Vec<_>>();
            counted.insert("promptTokenDetails".into(), details.into());
        }
        response["usageMetadata"] = counted.into();
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(items: Vec<Item>, f: impl FnOnce(&mut Input)) -> Result<(String, Value), Fault> {
        let mut input = Input::new(items);
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

    fn texts(texts: &[&str]) -> Vec<Item> {
        texts.iter().map(|t| Item::Text((*t).into())).collect()
    }

    fn request(path: &str, body: Value) -> Result<Input, Fault> {
        let head = http::Request::post(path).body(()).expect("a request");
        read_request(&head.into_parts().0, body.to_string().as_bytes())
    }

    #[test]
    fn every_text_is_one_request_of_one_batch() {
        let (path, body) = call(texts(&["a", "b"]), |i| i.dimensions = Some(768)).expect("a call");
        assert_eq!(path, "/models/gemini-embedding-001:batchEmbedContents");
        let request = |text: &str| {
            json!({ "model": "models/gemini-embedding-001",
                    "content": { "parts": [{ "text": text }] },
                    "outputDimensionality": 768 })
        };
        assert_eq!(body, json!({ "requests": [request("a"), request("b")] }));
        let tokens = vec![Item::Native(Native::new(json!([1])))];
        assert!(call(tokens, |_| {}).is_err());
        assert!(call(texts(&["a"]), |i| i.model = None).is_err());
    }

    #[test]
    fn parts_are_one_request_of_parts() {
        let media = |mime: &str, source| Part::Media(Media::new(mime, source));
        let bytes = MediaSource::Bytes(Bytes::from_static(b"abc"));
        let url = |u: &str| MediaSource::Url(u.into());
        let parts = vec![
            Part::Text("a".into()),
            media("video/mp4", bytes),
            media("image/*", url("https://x.test/i")),
        ];
        let (_, body) = call(vec![Item::Parts(Parts::new(parts))], |_| {}).expect("a call");
        let sent = json!([
            { "text": "a" },
            { "inlineData": { "mimeType": "video/mp4", "data": "YWJj" } },
            { "fileData": { "fileUri": "https://x.test/i" } },
        ]);
        assert_eq!(body["requests"][0]["content"]["parts"], sent);
    }

    #[test]
    fn an_answer_is_its_vectors_and_the_tokens_gemini_counts() {
        let read = |answer: Value| {
            let uri = Uri::from_static("/v1beta/models/m:batchEmbedContents");
            let mut decoder = read_response(call_of(&uri).map(|(model, _)| model));
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

    #[test]
    fn a_clients_requests_are_read_and_sent_on_as_they_were() {
        let asked = json!({ "requests": [
            { "model": "models/m", "content": { "parts": [{ "text": "a" }] },
              "outputDimensionality": 768 },
            { "model": "models/m", "taskType": "RETRIEVAL_QUERY", "output_dimensionality": 768,
              "content": { "parts": [
                { "text": "b" },
                { "inline_data": { "mime_type": "image/png", "data": "YWJj" } },
                { "fileData": { "fileUri": "https://x.test/v" } },
              ] } },
        ] });
        let input = request("/v1beta/models/m:batchEmbedContents", asked).expect("a request");
        assert_eq!(input.model.as_deref(), Some("m"));
        assert_eq!(input.dimensions, Some(768));
        assert_eq!(input.items[0], Item::Text("a".into()));
        let (_, body) = call(input.items, |i| i.dimensions = Some(768)).expect("a call");
        let sent = json!({ "model": "models/gemini-embedding-001",
            "taskType": "RETRIEVAL_QUERY", "outputDimensionality": 768,
            "content": { "parts": [
                { "text": "b" },
                { "inlineData": { "mimeType": "image/png", "data": "YWJj" } },
                { "fileData": { "fileUri": "https://x.test/v" } },
            ] } });
        assert_eq!(body["requests"][1], sent);

        let one = json!({ "content": { "parts": [{ "text": "a" }] } });
        let input = request("/v1beta/models/m:embedContent", one).expect("a request");
        assert_eq!(input.items, [Item::Text("a".into())]);
        let odd = json!({ "content": { "parts": [{ "functionCall": {} }] } });
        assert!(request("/v1beta/models/m:embedContent", odd).is_err());
        assert!(request("/v1beta/models/m:batchEmbedContents", json!({})).is_err());
    }

    #[test]
    fn a_client_is_answered_its_vectors_by_index_and_the_usage() {
        let events = [
            Output::Embedding(Embedding::new(1, vec![0.5])),
            Output::Embedding(Embedding::new(0, vec![0.1])),
            Output::Usage(Usage::new(1059, 0)),
            Output::InputTokens(Modality::Video, 1056),
            Output::InputTokens(Modality::Text, 3),
        ];
        let usage = json!({ "promptTokenCount": 1059, "promptTokenDetails": [
            { "modality": "VIDEO", "tokenCount": 1056 },
            { "modality": "TEXT", "tokenCount": 3 }] });
        let batch = json!({ "embeddings": [{ "values": [0.1] }, { "values": [0.5] }],
            "usageMetadata": usage });
        assert_eq!(response_of(&events, true), batch);
        let one = json!({ "embedding": { "values": [0.1] }, "usageMetadata": usage });
        assert_eq!(response_of(&events, false), one);
    }
}
