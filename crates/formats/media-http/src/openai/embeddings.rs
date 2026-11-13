//! OpenAI Embeddings: `POST /embeddings` (JSON), answered whole in JSON.

use super::{
    Fields, Form, as_usage_extras, b64, bind_client, bind_provider, invalid, json_bytes,
    merge_missing, missing, post_to, rest_of, sends, set, take_count, take_string, unb64,
    unreadable, whole_json,
};
use respan_gateway_conversation::Usage;
use respan_gateway_core::{Connection, Fault, Format, Native, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::{self, Method, request::Parts as RequestHead};
use respan_gateway_http::path::path_ends_with;
use respan_gateway_http::{All, ClientIo, Decoder, Encoder, ProviderIo, Whole, native::Json};
use respan_gateway_http::{Connect, Upstream};
use respan_gateway_media::embedding::{Embedding, Input, Item, Output, Start};
use serde_json::{Map, Value};

const NAME: &str = "openai-embeddings";

type Key = crate::Own<OpenAiEmbeddings>;
const EMBEDDINGS: &str = "/embeddings";

/// OpenAI Embeddings, as OpenAI or a compatible vendor serves it: a `POST`
/// to a path ending in `/embeddings`.
///
/// The input is a text, a list of texts, a list of token ids, a list of
/// those, or a list of parts (objects such as `{"type": "image", ..}`),
/// relayed as written; a single item is written as it is, several as a
/// list. A vector
/// is decoded from a list of numbers or from base64 (little-endian `f32`s),
/// whichever the provider answers, and written as the client's
/// `encoding_format` asks, `float` unless it says `base64`. The field is
/// relayed to the provider as the client wrote it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenAiEmbeddings;

impl OpenAiEmbeddings {
    /// The format's name, `openai-embeddings`.
    pub const NAME: &'static str = NAME;
}

impl Format<ClientIo, Input, Output> for OpenAiEmbeddings {
    fn name(&self) -> &str {
        NAME
    }

    fn matches(&self, io: &ClientIo) -> bool {
        let request = io.request();
        request.method() == Method::POST && path_ends_with(request.uri(), EMBEDDINGS)
    }

    fn bind(&self, io: ClientIo) -> Result<Connection<Input, Output>, Fault> {
        Ok(bind_client(io, decode_request, response_encoder))
    }
}

/// Its spec's: a bearer key, over HTTP, beside its base URL.
impl Upstream for OpenAiEmbeddings {
    fn name(&self) -> &str {
        NAME
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for OpenAiEmbeddings {
    fn name(&self) -> &str {
        NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        Ok(bind_provider(io, NAME, post, |_, _| Some(read_response())))
    }
}

/// A client's request.
fn decode_request(_: &RequestHead, form: &Form) -> Result<Input, Fault> {
    let Fields { mut map, files } = form.fields()?;
    if let Some((name, _)) = files.first() {
        return Err(invalid(name, "is not a file this endpoint takes"));
    }
    let items = map.remove("input").ok_or_else(|| missing("input"))?;
    let mut input = Input::new(read_items(&items)?);
    input.model = take_string(&mut map, "model")?;
    input.dimensions = take_count(&mut map, "dimensions")?;
    input.native = Native::json::<Key>(map);
    Ok(input)
}

/// The items of a request's `input`: its texts, else, as token ids or
/// parts, the format's own, as it came; a list of token lists is one each.
fn read_items(v: &Value) -> Result<Vec<Item>, Fault> {
    let items = match v {
        Value::String(text) => vec![Item::Text(text.clone())],
        Value::Array(all) if all.iter().all(Value::is_string) => all
            .iter()
            .filter_map(Value::as_str)
            .map(|text| Item::Text(text.to_owned()))
            .collect(),
        Value::Array(all) if all.iter().all(Value::is_array) => all
            .iter()
            .map(|ids| Item::Native(Native::json::<Key>(ids.clone())))
            .collect(),
        Value::Array(_) => vec![Item::Native(Native::json::<Key>(v.clone()))],
        _ => return Err(invalid("input", "must be a string or an array")),
    };
    match items.is_empty() {
        true => Err(invalid("input", "must not be empty")),
        false => Ok(items),
    }
}

/// The call of `input`, in JSON.
fn post(head: &mut RequestHead, input: &Input) -> Result<Box<dyn Encoder<Input>>, Fault> {
    post_to(head, EMBEDDINGS, "application/json")?;
    let mut v = Map::new();
    set(&mut v, "model", input.model.clone());
    v.insert("input".into(), items_json(&input.items)?);
    set(&mut v, "dimensions", input.dimensions);
    merge_missing(&mut v, input.native.fields::<Key>());
    Ok(sends(json_bytes(&v)?))
}

/// The `input` of items: one as it is, several as a list of one kind.
fn items_json(items: &[Item]) -> Result<Value, Fault> {
    let item = |item: &Item| match item {
        Item::Text(text) => Value::from(text.clone()),
        // WARN: another format's item has no form here; it is sent as null.
        Item::Native(n) => n.value::<Key>().cloned().unwrap_or_default(),
        _ => Value::Null,
    };
    if let [only] = items {
        return Ok(item(only));
    }
    let same = |pair: &[Item]| std::mem::discriminant(&pair[0]) == std::mem::discriminant(&pair[1]);
    match items.windows(2).all(same) {
        true => Ok(items.iter().map(item).collect::<Vec<_>>().into()),
        false => Err(Fault::bad_request(
            "an input cannot mix texts with tokens or parts",
        )),
    }
}

/// The decoder of a provider's answer: one JSON body.
fn read_response() -> Box<dyn Decoder<Output>> {
    Box::new(Whole::new(decode_whole))
}

/// The events of a whole JSON response.
fn decode_whole(raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
    let v = whole_json(NAME, raw, |v| v["data"].is_array())?;
    let mut start = Start::new(v["model"].as_str().map(Into::into));
    start.native = Native::json::<Key>(rest_of(&v, &["object", "data", "model", "usage"]));
    out.push(Output::Start(start));
    for (at, item) in v["data"].as_array().into_iter().flatten().enumerate() {
        let index = item["index"].as_u64().and_then(|i| u32::try_from(i).ok());
        let vector = vector_of(&item["embedding"])?;
        let mut embedding = Embedding::new(index.unwrap_or(at as u32), vector);
        embedding.native = Native::json::<Key>(rest_of(item, &["object", "index", "embedding"]));
        out.push(Output::Embedding(embedding));
    }
    if v["usage"].is_object() {
        let count = |key: &str| match &v["usage"][key] {
            Value::Null => Ok(0),
            n => n.as_u64().ok_or_else(|| {
                Fault::upstream(format!("a usage count is not a whole number: {n}"))
            }),
        };
        out.push(Output::Usage(Usage::new(count("prompt_tokens")?, 0)));
        let extras = rest_of(&v["usage"], &["prompt_tokens", "total_tokens"]);
        out.extend(super::usage_extras::<Key>(extras).map(Output::Native));
    }
    Ok(())
}

/// A vector, as a list of numbers or as base64 of little-endian `f32`s.
fn vector_of(v: &Value) -> Result<Vec<f32>, Fault> {
    match v {
        Value::Array(numbers) => numbers
            .iter()
            .map(|n| n.as_f64().map(|n| n as f32))
            .collect::<Option<_>>()
            .ok_or_else(|| unreadable(NAME, "holds a vector of other than numbers")),
        Value::String(text) => unb64(text)
            .filter(|bytes| bytes.len() % 4 == 0)
            .map(|bytes| {
                let (floats, _) = bytes.as_chunks::<4>();
                floats.iter().map(|f| f32::from_le_bytes(*f)).collect()
            })
            .ok_or_else(|| unreadable(NAME, "holds a vector not in base64 of `f32`s")),
        _ => Err(unreadable(NAME, "holds an item with no vector")),
    }
}

/// The encoder of a response to a client's `request`, with the vectors
/// encoded as it asked.
fn response_encoder(
    _request: &http::Request<Bytes>,
    asked: &Map<String, Value>,
    head: &mut http::response::Parts,
) -> Box<dyn Encoder<Output>> {
    super::content_type(head, "application/json");
    let base64 = asked.get("encoding_format").and_then(Value::as_str) == Some("base64");
    Box::new(All::new(move |events: &[Output]| {
        encode_whole(events, base64)
    }))
}

/// A vector as the client asked: as base64 of little-endian `f32`s, or as
/// a list of numbers.
fn vector_json(vector: &[f32], base64: bool) -> Value {
    if base64 {
        let bytes: Vec<u8> = vector.iter().flat_map(|f| f.to_le_bytes()).collect();
        return b64(&bytes).into();
    }
    // A number as its shortest `f32` text, not as the `f64` it widens to.
    let number = |f: &f32| {
        f.to_string()
            .parse::<f64>()
            .map_or(Value::Null, Value::from)
    };
    vector.iter().map(number).collect::<Vec<_>>().into()
}

/// A whole response of `events`.
fn encode_whole(events: &[Output], base64: bool) -> Result<Bytes, Fault> {
    let (mut start, mut data, mut usage, mut extras) = (None, Vec::new(), None, Map::new());
    for event in events {
        match event {
            Output::Start(s) => start = Some(s),
            Output::Embedding(e) => {
                let mut item = Map::new();
                item.insert("object".into(), "embedding".into());
                item.insert("index".into(), e.index.into());
                item.insert("embedding".into(), vector_json(&e.vector, base64));
                merge_missing(&mut item, e.native.fields::<Key>());
                data.push(Value::Object(item));
            }
            Output::Usage(u) => *usage.get_or_insert_with(Usage::default) += u,
            // A body has no place for another event.
            Output::Native(n) => {
                if let Some(more) = n.value::<Key>().and_then(as_usage_extras) {
                    merge_missing(&mut extras, more);
                }
            }
            _ => {}
        }
    }
    let mut body = Map::new();
    body.insert("object".into(), "list".into());
    body.insert("data".into(), data.into());
    set(&mut body, "model", start.and_then(|s| s.model.clone()));
    if let Some(usage) = usage {
        let mut u = Map::new();
        u.insert("prompt_tokens".into(), usage.input_tokens.into());
        u.insert(
            "total_tokens".into(),
            usage
                .input_tokens
                .saturating_add(usage.output_tokens)
                .into(),
        );
        merge_missing(&mut u, &extras);
        body.insert("usage".into(), u.into());
    }
    if let Some(start) = start {
        merge_missing(&mut body, start.native.fields::<Key>());
    }
    json_bytes(&body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_core::FaultKind;
    use serde_json::json;

    fn items(input: Value) -> Result<Vec<Item>, Fault> {
        read_items(&input)
    }

    #[test]
    fn every_form_of_input_is_read_and_written_back() {
        let text = |t: &str| Item::Text(t.to_owned());
        let native = |v: &Value| vec![Item::Native(Native::json::<Key>(v.clone()))];
        let (tokens, lists) = (json!([1, 2]), json!([[1], [2, 3]]));
        let parts = json!([{"type": "image", "url": "u"}]);
        let cases = [
            (json!("a"), vec![text("a")], json!("a")),
            (json!(["a"]), vec![text("a")], json!("a")),
            (
                json!(["a", "b"]),
                vec![text("a"), text("b")],
                json!(["a", "b"]),
            ),
            (tokens.clone(), native(&tokens), tokens),
            (
                lists.clone(),
                [json!([1]), json!([2, 3])]
                    .iter()
                    .flat_map(native)
                    .collect(),
                lists,
            ),
            (parts.clone(), native(&parts), parts),
        ];
        for (asked, read, written) in cases {
            let read_back = items(asked.clone()).expect("items");
            // Its natives say what they hold as they are written back.
            assert_eq!(format!("{read_back:?}"), format!("{read:?}"), "{asked}");
            assert_eq!(items_json(&read_back).expect("input"), written);
        }
    }

    #[test]
    fn an_input_of_no_items_or_odd_ones_is_refused() {
        for asked in [json!([]), json!(7)] {
            let refused = items(asked.clone());
            assert!(refused.is_err_and(|f| f.is(FaultKind::Client)), "{asked}");
        }
        let mixed = [
            Item::Text("a".into()),
            Item::Native(Native::json::<Key>(json!([1]))),
        ];
        assert!(items_json(&mixed).is_err());
    }

    #[test]
    fn a_vector_is_read_and_written_in_either_encoding() {
        let vector = [0.1, -2.5, 1e-7, 3.0];
        let floats = vector_json(&vector, false);
        assert_eq!(floats, json!([0.1, -2.5, 1e-7, 3.0]));
        let base64 = vector_json(&vector, true);
        assert_eq!(vector_of(&floats).expect("floats"), vector);
        assert_eq!(vector_of(&base64).expect("base64"), vector);
        assert!(vector_of(&json!("AAA=")).is_err());
    }

    #[test]
    fn a_response_is_written_with_its_usage_and_extras() {
        let raw = json!({
            "object": "list",
            "model": "m",
            "data": [{"object": "embedding", "index": 0, "embedding": [1.5], "x": 1}],
            "usage": {"prompt_tokens": 3, "total_tokens": 3, "y": {"z": 1}},
            "w": true,
        });
        let mut events = Vec::new();
        decode_whole(Bytes::from(raw.to_string()), &mut events).expect("events");
        assert!(events.contains(&Output::Usage(Usage::new(3, 0))));
        let written = encode_whole(&events, false).expect("a body");
        let written: Value = serde_json::from_slice(&written).unwrap_or_default();
        assert_eq!(written, raw);
    }
}
