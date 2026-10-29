//! Amazon Bedrock's InvokeModel for embeddings, over HTTP: a provider format
//! of the [`embedding`](respan_gateway_media::embedding) kind,
//! [`BedrockEmbeddings`]. A provider's base URL is its region's runtime,
//! `https://bedrock-runtime.{region}.amazonaws.com`, its key a Bedrock API key
//! as a bearer token.

use percent_encoding::percent_decode_str;
use respan_gateway_conversation::Usage;
use respan_gateway_conversation_http::fields::invalid;
use respan_gateway_core::{Connection, Fault, Format, NativeFields, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::header::{CONTENT_TYPE, HeaderValue};
use respan_gateway_http::http::{self, request::Parts as RequestHead};
use respan_gateway_http::path::{escape, set_path};
use respan_gateway_http::{Connect, Decoder, Encoder, ProviderIo, Upstream, Whole};
use respan_gateway_media::embedding::{Embedding, Input, Item, Output, Start};
use serde_json::{Map, Value, json};

use crate::edge::{bind_provider, sends};
use crate::openai::OpenAiEmbeddings;

const NAME: &str = "bedrock-embeddings";
/// The header Bedrock counts a call's input tokens in.
const INPUT_TOKENS: &str = "x-amzn-bedrock-input-token-count";
/// What Cohere embeds a text as unless the client says: a stored document.
const SEARCH_DOCUMENT: &str = "search_document";

/// Amazon Bedrock's InvokeModel for embeddings, `POST /model/{model}/invoke`:
/// Cohere Embed (`cohere.embed-*`) and Amazon Titan Embeddings
/// (`amazon.titan-embed-text-*`, the texts of `amazon.titan-embed-image-v1`),
/// an inference profile's id (`us.cohere.embed-v4:0`) as it is.
///
/// Cohere embeds every text in one call, as a `search_document` unless the
/// client's `input_type` (an [`OpenAiEmbeddings`] field) says otherwise; v4
/// takes `dimensions` as `output_dimension`. Titan embeds one text per call,
/// so a request of several is refused; v2 and Multimodal take `dimensions`.
/// Vectors are asked for as floats. Usage is the input tokens Bedrock
/// counts: Titan's `inputTextTokenCount`, else the
/// `x-amzn-bedrock-input-token-count` header.
///
/// Token ids, parts, an end user's id, a `dimensions` the model takes none
/// of, and other fields of another format are refused, 400, before the call.
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
            _ => return None,
        })
    }
}

/// A bearer key, over HTTP, beside its base URL.
impl Upstream for BedrockEmbeddings {
    fn name(&self) -> &str {
        NAME
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for BedrockEmbeddings {
    fn name(&self) -> &str {
        NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        Ok(bind_provider(io, NAME, post, |head, call| {
            let tokens = head.headers.get(INPUT_TOKENS).and_then(|v| v.to_str().ok());
            let tokens = tokens.and_then(|n| n.parse::<u64>().ok());
            Some(read_response(model_of(call.uri()), tokens))
        }))
    }
}

/// The call of `input`: its model's InvokeModel, in its family's JSON.
fn post(head: &mut RequestHead, input: &Input) -> Result<Box<dyn Encoder<Input>>, Fault> {
    let model = input.model.as_deref().filter(|m| !m.is_empty());
    let model = model.ok_or_else(|| invalid("model", "is needed by Bedrock"))?;
    let family = Family::of(model).ok_or_else(|| {
        invalid(
            "model",
            "is not a Bedrock embedding model (Cohere Embed or Titan)",
        )
    })?;
    let texts = (input.items.iter())
        .map(|item| match item {
            Item::Text(text) => Ok(text.as_str()),
            _ => Err(invalid("input", "must be texts for Bedrock")),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if input.user.is_some() {
        return Err(invalid("user", "has no field on Bedrock"));
    }
    let input_type = input_type(&input.native)?;
    let no_dimensions = || invalid("dimensions", "is not taken by this model");
    let dimensions = input.dimensions;
    let body = match family {
        Family::Cohere { v4 } => {
            let mut v = json!({
                "texts": texts,
                "input_type": input_type.unwrap_or(SEARCH_DOCUMENT),
                "embedding_types": ["float"],
            });
            match (dimensions, v4) {
                (None, _) => {}
                (Some(n), true) => v["output_dimension"] = n.into(),
                (Some(_), false) => return Err(no_dimensions()),
            }
            v
        }
        _ if input_type.is_some() => {
            return Err(invalid("input_type", "is taken by Cohere models only"));
        }
        _ if texts.len() != 1 => {
            return Err(invalid(
                "input",
                "must be one text: Titan embeds one per call",
            ));
        }
        Family::TitanText { v2 } => match (dimensions, v2) {
            (None, _) => json!({ "inputText": texts[0] }),
            (Some(n), true) => json!({ "inputText": texts[0], "dimensions": n }),
            (Some(_), false) => return Err(no_dimensions()),
        },
        Family::TitanMultimodal => match dimensions {
            None => json!({ "inputText": texts[0] }),
            Some(n) => json!({ "inputText": texts[0],
                               "embeddingConfig": { "outputEmbeddingLength": n } }),
        },
    };
    set_path(head, &format!("/model/{}/invoke", escape(model)))?;
    let json = HeaderValue::from_static("application/json");
    head.headers.insert(CONTENT_TYPE, json);
    let body = serde_json::to_vec(&body).map_err(|e| Fault::internal(e.to_string()))?;
    Ok(sends(body.into()))
}

/// The client's `input_type`, an OpenAI Embeddings field Cohere reads; its
/// `encoding_format` is its own edge's. Any other field is refused.
fn input_type(native: &Option<NativeFields>) -> Result<Option<&str>, Fault> {
    let Some(native) = native.as_ref().filter(|n| !n.is_empty()) else {
        return Ok(None);
    };
    let fields: &Map<String, Value> = &native.fields;
    let own = &*native.format == OpenAiEmbeddings::NAME;
    let other = fields
        .keys()
        .find(|k| !own || !["input_type", "encoding_format"].contains(&k.as_str()));
    if let Some(key) = other {
        return Err(invalid(key, "has no field on Bedrock"));
    }
    match fields.get("input_type") {
        None => Ok(None),
        Some(Value::String(kind)) => Ok(Some(kind)),
        Some(_) => Err(invalid("input_type", "must be a string")),
    }
}

/// The model a call at `uri`, `/model/{model}/invoke`, is of.
fn model_of(uri: &http::Uri) -> Option<String> {
    let model = uri.path().strip_suffix("/invoke")?.rsplit('/').next()?;
    Some(percent_decode_str(model).decode_utf8_lossy().into_owned())
}

/// The decoder of an answer to a call of `model`, its input tokens as the
/// header counts them.
fn read_response(model: Option<String>, tokens: Option<u64>) -> Box<dyn Decoder<Output>> {
    Box::new(Whole::new(move |raw: Bytes, out: &mut Vec<Output>| {
        let v: Value = serde_json::from_slice(&raw)
            .map_err(|_| Fault::upstream("Bedrock's answer is not JSON"))?;
        // Cohere's vectors by type, or bare; Titan's one.
        let embeddings = &v["embeddings"];
        let vectors: Vec<&Value> = match (&embeddings["float"], &v["embedding"]) {
            (Value::Array(all), _) => all.iter().collect(),
            (_, one @ Value::Array(_)) => vec![one],
            _ => embeddings.as_array().into_iter().flatten().collect(),
        };
        if vectors.is_empty() {
            return Err(Fault::upstream("Bedrock's answer holds no vector"));
        }
        out.push(Output::Start(Start::new(model.clone())));
        for (at, vector) in vectors.into_iter().enumerate() {
            let numbers = vector.as_array().into_iter().flatten();
            let vector = numbers
                .map(|n| n.as_f64().map(|n| n as f32))
                .collect::<Option<_>>();
            let vector = vector.ok_or_else(|| Fault::upstream("a vector is not numbers"))?;
            out.push(Output::Embedding(Embedding::new(at as u32, vector)));
        }
        let counted = v["inputTextTokenCount"].as_u64().or(tokens);
        out.extend(counted.map(|n| Output::Usage(Usage::new(n, 0))));
        Ok(())
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(
        model: &str,
        items: &[&str],
        f: impl FnOnce(&mut Input),
    ) -> Result<(String, Value), Fault> {
        let mut input = Input::new(
            items
                .iter()
                .map(|t| Item::Text((*t).into()))
                .collect::<Vec<_>>(),
        );
        input.model = Some(model.into());
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

    fn openai(fields: Value) -> Option<NativeFields> {
        Some(NativeFields::new(
            OpenAiEmbeddings::NAME,
            fields.as_object().cloned().unwrap_or_default(),
        ))
    }

    #[test]
    fn cohere_embeds_every_text_in_one_call() {
        let (path, body) = call("us.cohere.embed-v4:0", &["a", "b"], |i| {
            i.dimensions = Some(256);
            i.native = openai(json!({ "input_type": "search_query", "encoding_format": "base64" }));
        })
        .expect("a call");
        assert_eq!(path, "/model/us.cohere.embed-v4%3A0/invoke");
        assert_eq!(
            body,
            json!({ "texts": ["a", "b"], "input_type": "search_query",
                    "embedding_types": ["float"], "output_dimension": 256 })
        );
        let (_, body) = call("cohere.embed-english-v3", &["a"], |_| {}).expect("a call");
        assert_eq!(body["input_type"], SEARCH_DOCUMENT);
        assert!(
            call("cohere.embed-english-v3", &["a"], |i| i.dimensions =
                Some(8))
            .is_err()
        );
    }

    #[test]
    fn titan_embeds_one_text_and_refuses_more() {
        let (_, body) = call("amazon.titan-embed-text-v2:0", &["a"], |i| {
            i.dimensions = Some(512)
        })
        .expect("a call");
        assert_eq!(body, json!({ "inputText": "a", "dimensions": 512 }));
        let (_, body) = call("amazon.titan-embed-image-v1", &["a"], |i| {
            i.dimensions = Some(384)
        })
        .expect("a call");
        assert_eq!(
            body["embeddingConfig"],
            json!({ "outputEmbeddingLength": 384 })
        );
        let refused = call("amazon.titan-embed-text-v2:0", &["a", "b"], |_| {});
        assert!(refused.is_err_and(|f| f.message().contains("one text")));
        assert!(
            call("amazon.titan-embed-text-v1", &["a"], |i| i.dimensions =
                Some(8))
            .is_err()
        );
    }

    #[test]
    fn what_bedrock_has_no_field_for_is_refused() {
        assert!(call("amazon.nova-micro-v1:0", &["a"], |_| {}).is_err());
        assert!(call("cohere.embed-v4:0", &["a"], |i| i.user = Some("u".into())).is_err());
        let foreign = call("cohere.embed-v4:0", &["a"], |i| {
            i.native = openai(json!({ "seed": 1 }))
        });
        assert!(foreign.is_err_and(|f| f.message().contains("seed")));
        let titan = call("amazon.titan-embed-text-v2:0", &["a"], |i| {
            i.native = openai(json!({ "input_type": "search_query" }))
        });
        assert!(titan.is_err());
    }

    fn read(model: &str, tokens: Option<u64>, answer: Value) -> Vec<Output> {
        let mut decoder = read_response(Some(model.into()), tokens);
        let mut out = Vec::new();
        decoder
            .decode(answer.to_string().into(), &mut out)
            .expect("read");
        decoder.end(Ok(()), &mut out).expect("ended");
        out
    }

    #[test]
    fn an_answer_is_its_vectors_and_the_tokens_bedrock_counts() {
        let cohere = read(
            "cohere.embed-v4:0",
            Some(7),
            json!({ "id": "x",
            "embeddings": { "float": [[0.5, 1.0], [2.0, 0.25]] }, "texts": ["a", "b"] }),
        );
        assert_eq!(
            cohere,
            [
                Output::Start(Start::new(Some("cohere.embed-v4:0".into()))),
                Output::Embedding(Embedding::new(0, vec![0.5, 1.0])),
                Output::Embedding(Embedding::new(1, vec![2.0, 0.25])),
                Output::Usage(Usage::new(7, 0)),
            ]
        );
        let titan = read(
            "amazon.titan-embed-text-v2:0",
            Some(9),
            json!({ "embedding": [0.5], "inputTextTokenCount": 3 }),
        );
        assert_eq!(
            titan[1..],
            [
                Output::Embedding(Embedding::new(0, vec![0.5])),
                Output::Usage(Usage::new(3, 0))
            ]
        );
        let uri: http::Uri = "/model/us.cohere.embed-v4%3A0/invoke"
            .parse()
            .expect("a path");
        assert_eq!(model_of(&uri).as_deref(), Some("us.cohere.embed-v4:0"));
    }
}
