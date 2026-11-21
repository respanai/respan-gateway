//! Add a protocol by writing its Format: here, OpenAI embeddings.
//!
//! A protocol is a kind and a format. The kind is plain data, `EmbedRequest`
//! in and `Embeddings` out; the format reads and writes it as HTTP, once for
//! the client edge and once for the provider edge. Plugins then read and edit
//! its values, whatever bytes they came in as.
//!
//! The format writes each value from its fields, so a message relays by what
//! it says, not by its bytes; fields the kind has no form for ride along in
//! `rest`. (The gateway ships a full OpenAI embeddings format in
//! `respan-gateway-media-http`; this one is small on purpose.)
//!
//! Run it with `cargo run -p respan-gateway-examples --example custom_format`.
//! The client asks for `text-embedding-3-large`; the `PinModel` plugin edits
//! the request to `text-embedding-3-small`. Look for the pinned model and the
//! kept `dimensions` in what was sent, the answer rewritten from its fields,
//! and the usage the `Metered` plugin read from the provider's answer.

use std::sync::Arc;

use mockllm::{Answer, FakeOpenAi};
use parking_lot::Mutex;
use respan_gateway_conversation::Usage;
use respan_gateway_core::{
    ClientContext, Connection, Fault, Format, Gateway, Plugin, Provider, ProviderContext, Response,
    async_trait,
};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::response::Parts as ResponseHead;
use respan_gateway_http::http::{self, HeaderMap, HeaderValue, Method, header::CONTENT_TYPE};
use respan_gateway_http::http_body_util::BodyExt;
use respan_gateway_http::{All, Body, ClientIo, ProviderIo, Whole, content};
use respan_gateway_http_transport::HttpTransport;
use serde_json::{Map, Value, json};

/// The most bytes of one message the gateway holds.
const LIMIT: usize = 32 << 20;

#[tokio::main]
async fn main() -> Result<(), Fault> {
    let openai = FakeOpenAi::start().await;
    openai.answer(Answer::raw(
        r#"{"object": "list", "data": [{"object": "embedding", "index": 0, "embedding": [0.5, -0.5]}],
 "model": "text-embedding-3-small", "usage": {"prompt_tokens": 1, "total_tokens": 1}}"#,
    ));
    let mut key = HeaderMap::new();
    key.insert("authorization", HeaderValue::from_static("Bearer sk-test"));
    let transport = HttpTransport::new(openai.base_url())?.with_headers(key)?;

    let metered = Metered::default();
    let gw = Gateway::new()
        .plugin(PinModel("text-embedding-3-small"))
        .plugin(metered.clone())
        .provider(Provider::new(
            "openai",
            OpenAiEmbeddings,
            transport.connect(),
        ))?
        .client::<EmbedRequest, _, _>(OpenAiEmbeddings)?
        .target("openai")?;

    let asked = r#"{"model": "text-embedding-3-large", "input": ["hi"], "dimensions": 2}"#;
    let mut req = http::Request::new(Body::full(asked));
    *req.method_mut() = Method::POST;
    *req.uri_mut() = http::Uri::from_static("/v1/embeddings");
    let (resp, _) = respan_gateway_http::handle(&gw, req, LIMIT).await;
    let status = resp.status();
    let answered = resp.into_body().collect().await?.to_bytes();
    gw.idle().await;

    println!("client asked:  {asked}");
    for sent in openai.requests() {
        println!("provider got:  {}", String::from_utf8_lossy(&sent.raw));
    }
    println!(
        "client got:    {status} {}",
        String::from_utf8_lossy(&answered)
    );
    println!("metered usage: {:?}", *metered.0.lock());
    Ok(())
}

/// The kind's input: texts to embed.
#[derive(Debug, Clone, Default, PartialEq)]
struct EmbedRequest {
    model: Option<String>,
    input: Vec<String>,
    /// The other fields of the body, such as `dimensions`.
    rest: Map<String, Value>,
}

/// The kind's output: a vector per input, and the usage.
#[derive(Debug, Clone, Default, PartialEq)]
struct Embeddings {
    vectors: Vec<Vec<f64>>,
    usage: Usage,
    /// The other fields of the body, such as `model`.
    rest: Map<String, Value>,
}

const NAME: &str = "openai-embeddings";

/// OpenAI's `POST /v1/embeddings`.
#[derive(Debug, Clone)]
struct OpenAiEmbeddings;

// The client edge: which requests are this protocol's, how to read the
// request and how to write the answer and errors.
impl Format<ClientIo, EmbedRequest, Embeddings> for OpenAiEmbeddings {
    fn name(&self) -> &str {
        NAME
    }

    fn matches(&self, io: &ClientIo) -> bool {
        io.request().method() == Method::POST
            && respan_gateway_http::path::clean_path(io.request().uri().path())
                .ends_with("/embeddings")
    }

    fn bind(&self, io: ClientIo) -> Result<Connection<EmbedRequest, Embeddings>, Fault> {
        Ok(io.with_error(error).bind(
            // The request's one body, read into its one input.
            |_| {
                Box::new(|raw: Bytes, out: &mut Vec<EmbedRequest>| {
                    out.push(read_request(raw)?);
                    Ok(())
                })
            },
            // The response written whole, once it ended.
            |reply| {
                let mut head = reply.head();
                content::default_content_type(&mut head, "application/json");
                Ok(reply.write(head, Box::new(All::new(write_embeddings))))
            },
        ))
    }
}

// The provider edge: how to write the call and read the answer.
impl Format<ProviderIo, Embeddings, EmbedRequest> for OpenAiEmbeddings {
    fn name(&self) -> &str {
        NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Embeddings, EmbedRequest>, Fault> {
        Ok(io.bind(
            // The call's head, and its one input written as its body.
            |head, request: Option<&EmbedRequest>| {
                if request.is_none() {
                    return Err(Fault::config(format!(
                        "a call in {NAME} carries one body, not streamed inputs"
                    )));
                }
                head.method = Method::POST;
                respan_gateway_http::path::set_path(head, "/embeddings")?;
                head.headers
                    .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
                Ok(Box::new(|r: &EmbedRequest, out: &mut Vec<Bytes>| {
                    out.push(write_request(r)?);
                    Ok(())
                }))
            },
            // The answer read whole into its one output.
            |_, _| {
                Some(Box::new(Whole::new(
                    |raw: Bytes, out: &mut Vec<Embeddings>| {
                        out.push(read_embeddings(raw)?);
                        Ok(())
                    },
                )))
            },
        ))
    }
}

/// A client's request body, read into its fields.
fn read_request(raw: Bytes) -> Result<EmbedRequest, Fault> {
    let mut v: Map<String, Value> =
        serde_json::from_slice(&raw).map_err(|_| Fault::bad_request("not a JSON object"))?;
    let model = v.remove("model").and_then(|m| m.as_str().map(Into::into));
    let texts_only = || Fault::bad_request("input must be a string or an array of strings");
    let input = match v.remove("input") {
        Some(Value::String(one)) => vec![one],
        // Texts; token arrays are not read here.
        Some(all) => serde_json::from_value(all).map_err(|_| texts_only())?,
        None => return Err(texts_only()),
    };
    // Base64 vectors are not read here either: the provider would be paid
    // for answers this kind cannot hold.
    if v.get("encoding_format").is_some_and(|f| f == "base64") {
        return Err(Fault::bad_request(
            "encoding_format base64 is not supported",
        ));
    }
    Ok(EmbedRequest {
        model,
        input,
        rest: v,
    })
}

/// A provider's answer, read into its fields: vectors in index order.
fn read_embeddings(raw: Bytes) -> Result<Embeddings, Fault> {
    let mut v: Map<String, Value> =
        serde_json::from_slice(&raw).map_err(|_| Fault::upstream("not a JSON object"))?;
    let mut said = Embeddings::default();
    let malformed = || Fault::upstream("invalid embedding indices or vectors");
    let data = v.remove("data").ok_or_else(malformed)?;
    let data = data.as_array().ok_or_else(malformed)?;
    let mut ordered = vec![None; data.len()];
    for item in data {
        let index = item["index"]
            .as_u64()
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| ordered.get_mut(index))
            .ok_or_else(malformed)?;
        // Base64 vectors are not read here: they are malformed for this kind.
        let vector: Vec<f64> =
            serde_json::from_value(item["embedding"].clone()).map_err(|_| malformed())?;
        if vector.is_empty() || index.replace(vector).is_some() {
            return Err(malformed());
        }
    }
    said.vectors = ordered
        .into_iter()
        .collect::<Option<_>>()
        .ok_or_else(malformed)?;
    let usage = v.remove("usage");
    if let Some(tokens) = usage.and_then(|u| u["prompt_tokens"].as_u64()) {
        said.usage.input_tokens += tokens;
    }
    said.rest = v;
    Ok(said)
}

/// The fault as this protocol's error body: the format's error writer.
fn error(fault: &Fault, _: &http::Request<Bytes>, head: &mut ResponseHead) -> Bytes {
    content::default_content_type(head, "application/json");
    Bytes::from(json!({ "error": { "message": fault.message() } }).to_string())
}

/// A request, written from its fields.
fn write_request(r: &EmbedRequest) -> Result<Bytes, Fault> {
    let mut v = r.rest.clone();
    v.insert("model".into(), json!(r.model));
    v.insert("input".into(), json!(r.input));
    Ok(Value::Object(v).to_string().into())
}

/// A response, written from its fields.
fn write_embeddings(one: &[Embeddings]) -> Result<Bytes, Fault> {
    let [e] = one else {
        return Err(Fault::internal("one response, one body"));
    };
    let mut v = e.rest.clone();
    let data = e.vectors.iter().enumerate();
    let data = data.map(|(i, v)| json!({ "object": "embedding", "index": i, "embedding": v }));
    v.insert("data".into(), data.collect());
    let tokens = e.usage.input_tokens;
    v.insert(
        "usage".into(),
        json!({ "prompt_tokens": tokens, "total_tokens": tokens }),
    );
    Ok(Value::Object(v).to_string().into())
}

/// A plugin on the new kind: every request uses `model`.
struct PinModel(&'static str);

#[async_trait]
impl Plugin for PinModel {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>, Fault> {
        if cx.is_kind::<EmbedRequest>() {
            let model = self.0;
            cx.filter(move |req: &mut EmbedRequest| {
                req.model = Some(model.into());
                Ok(())
            })?;
        }
        Ok(None)
    }
}

/// Adds up the usage the provider reports, with a filter of its kind on
/// each call.
#[derive(Clone, Default)]
struct Metered(Arc<Mutex<Usage>>);

#[async_trait]
impl Plugin for Metered {
    async fn on_provider(&self, px: &mut ProviderContext) -> Result<(), Fault> {
        let total = self.0.clone();
        px.filter(move |e: &mut Embeddings| {
            *total.lock() += &e.usage;
            Ok(())
        })
    }
}
