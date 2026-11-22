//! Rerank as Cohere's API has it, and Jina's, vLLM's and other servers'
//! after it: [`CohereRerank`], a format of the
//! [`rerank`](respan_gateway_media::rerank) kind.

use crate::edge::{bind_provider, posts};
use crate::openai::{as_usage_extras, usage_extras};
use respan_gateway_conversation::Usage;
use respan_gateway_conversation_http::Schema;
use respan_gateway_conversation_http::fields::{invalid, merge_missing, rest_of, set};
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::{Connection, Fault, Format, Native, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::header::{CONTENT_TYPE, HeaderValue};
use respan_gateway_http::http::{
    self, Method, request::Parts as RequestHead, response::Parts as ResponseHead,
};
use respan_gateway_http::native::Json;
use respan_gateway_http::path::path_ends_with;
use respan_gateway_http::{All, ClientIo, Connect, Decoder, Encoder, ProviderIo, Upstream, Whole};
use respan_gateway_media::rerank::{Document, Input, Output, Ranked, Start};
use serde_json::{Map, Value, json};

const NAME: &str = "cohere-rerank";

type Key = crate::Own<CohereRerank>;

const RERANK: &str = "/rerank";

/// Rerank as Cohere's API has it, which Jina, vLLM, Infinity and other
/// servers serve too: a `POST` to a path ending in `/rerank`, of a `query`
/// and its `documents`, answered whole in JSON.
///
/// A provider's base URL includes the version, such as
/// `https://api.cohere.com/v2`, `https://api.jina.ai/v1` or a vLLM server's
/// `http://localhost:8000/v1`, and its transport a bearer key.
///
/// A document is a text, or an object of only a `text`, which is read as
/// one; any other, such as Jina's `{"image": ..}` or an object of fields
/// for Cohere's `rank_fields`, is the format's own, sent on as written and
/// refused, 400, by a provider of another format. `top_n` is the kind's; a
/// request's other fields, such as `return_documents` and
/// `max_tokens_per_doc`, are sent on as written.
///
/// An answer's `results` are its documents' places, in order: each one's
/// `index`, `relevance_score`, and its `document`'s text if the provider
/// repeats it. Usage is `usage`'s `total_tokens`, as Jina and vLLM count
/// it, as input tokens, and its other fields are written back to a client
/// of the format as they came; so are Cohere's `meta`, its billed search
/// units, and an answer's `id`, its start's own fields. A refusal is in
/// OpenAI's error schema, as vLLM writes it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CohereRerank;

impl CohereRerank {
    /// The format's name, `cohere-rerank`.
    pub const NAME: &'static str = NAME;
}

/// Its spec's: a bearer key, over HTTP, beside its base URL.
impl Upstream for CohereRerank {
    fn name(&self) -> &str {
        NAME
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for CohereRerank {
    fn name(&self) -> &str {
        NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        Ok(bind_provider(io, NAME, post, |_, _| {
            Some(Box::new(Whole::new(decode_whole)) as Box<dyn Decoder<Output>>)
        }))
    }
}

/// The call of `input`: a `POST` to `/rerank` of its query and documents.
fn post(head: &mut RequestHead, input: &Input) -> Result<Box<dyn Encoder<Input>>, Fault> {
    let documents = input.documents.iter().map(document_json);
    let documents = documents.collect::<Result<Vec<_>, _>>()?;
    let mut v = Map::new();
    set(&mut v, "model", input.model.clone());
    v.insert("query".into(), input.query.clone().into());
    v.insert("documents".into(), documents.into());
    set(&mut v, "top_n", input.top_n);
    merge_missing(&mut v, input.native.fields::<Key>());
    posts(head, RERANK, &v)
}

/// A document as it is sent: a text as a string, the format's own as it
/// was written.
fn document_json(document: &Document) -> Result<Value, Fault> {
    match document {
        Document::Text(text) => Ok(text.clone().into()),
        Document::Native(native) => native.value::<Key>().cloned().ok_or_else(|| {
            invalid(
                "documents",
                "has a document of another format, which has no form here",
            )
        }),
        _ => Err(invalid("documents", "has a document of no form here")),
    }
}

/// The events of a provider's whole answer: its start, each document's
/// place, then its usage and the usage's other fields.
fn decode_whole(raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
    let v: Value = serde_json::from_slice(&raw)
        .map_err(|_| Fault::upstream(format!("the provider's {NAME} answer is not JSON")))?;
    let Some(results) = v["results"].as_array() else {
        return Err(Fault::upstream(format!(
            "the provider's success is not a response of {NAME}"
        )));
    };
    let mut start = Start::new(v["model"].as_str().map(Into::into));
    start.native = Native::json::<Key>(rest_of(&v, &["model", "results", "usage"]));
    out.push(Output::Start(start));
    for result in results {
        out.push(Output::Ranked(ranked(result)?));
    }
    let usage = &v["usage"];
    let counted = usage["total_tokens"]
        .as_u64()
        .or(usage["prompt_tokens"].as_u64());
    out.extend(counted.map(|n| Output::Usage(Usage::new(n, 0))));
    if usage.is_object() {
        let extras = rest_of(usage, &["total_tokens"]);
        out.extend(usage_extras::<Key>(extras).map(Output::Native));
    }
    Ok(())
}

/// A document's place in an answer: its index, its score, and its text, if
/// the answer repeats it as a string or as an object of only a `text`.
fn ranked(result: &Value) -> Result<Ranked, Fault> {
    let index = result["index"].as_u64().and_then(|i| u32::try_from(i).ok());
    let index = index.ok_or_else(|| Fault::upstream("a result names no document's index"))?;
    let score = result["relevance_score"].as_f64();
    let score = score.ok_or_else(|| Fault::upstream("a result has no relevance score"))?;
    let mut ranked = Ranked::new(index, score);
    let mut taken = vec!["index", "relevance_score"];
    if let Some(text) = text_of(&result["document"]) {
        ranked.text = Some(text.to_owned());
        taken.push("document");
    }
    ranked.native = Native::json::<Key>(rest_of(result, &taken));
    Ok(ranked)
}

/// The text of a document that is one: a string, or an object of only a
/// `text`.
fn text_of(v: &Value) -> Option<&str> {
    match v {
        Value::String(text) => Some(text),
        Value::Object(fields) if fields.len() == 1 => fields.get("text")?.as_str(),
        _ => None,
    }
}

// --- A client of the format: its request read, the kind's answer written.

impl Format<ClientIo, Input, Output> for CohereRerank {
    fn name(&self) -> &str {
        NAME
    }

    fn matches(&self, io: &ClientIo) -> bool {
        let request = io.request();
        request.method() == Method::POST && path_ends_with(request.uri(), RERANK)
    }

    fn bind(&self, io: ClientIo) -> Result<Connection<Input, Output>, Fault> {
        Ok(io.with_error(refusal).bind(
            |_| {
                Box::new(Whole::new(|raw: Bytes, out: &mut Vec<Input>| {
                    out.push(read_request(&raw)?);
                    Ok(())
                }))
            },
            |reply| {
                let mut head = reply.head();
                let encoder = response_encoder(&mut head);
                Ok(reply.write(head, encoder))
            },
        ))
    }
}

/// A refusal in OpenAI's error schema, as vLLM writes it.
fn refusal(fault: &Fault, request: &http::Request<Bytes>, head: &mut ResponseHead) -> Bytes {
    OpenAiChat.refusal(fault, request, head)
}

/// A client's request: its query, its documents, and how many to answer.
fn read_request(raw: &[u8]) -> Result<Input, Fault> {
    let mut v: Map<String, Value> = serde_json::from_slice(raw)
        .map_err(|_| Fault::bad_request("the request body is not a JSON object"))?;
    let query = match v.remove("query") {
        Some(Value::String(query)) => query,
        Some(_) => return Err(invalid("query", "must be a string")),
        None => return Err(invalid("query", "is required")),
    };
    let documents = match v.remove("documents") {
        Some(Value::Array(all)) if !all.is_empty() => {
            all.into_iter().map(read_document).collect::<Vec<_>>()
        }
        _ => return Err(invalid("documents", "must be a list of documents")),
    };
    let mut input = Input::new(query, documents);
    input.model = match v.remove("model") {
        None | Some(Value::Null) => None,
        Some(Value::String(model)) => Some(model),
        Some(_) => return Err(invalid("model", "must be a string")),
    };
    input.top_n = match v.remove("top_n") {
        None | Some(Value::Null) => None,
        Some(n) => Some(
            n.as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| invalid("top_n", "must be a whole number"))?,
        ),
    };
    input.native = Native::json::<Key>(v);
    Ok(input)
}

/// A client's document: a text if it is one, else the format's own.
fn read_document(v: Value) -> Document {
    match text_of(&v) {
        Some(text) => Document::Text(text.to_owned()),
        None => Document::Native(Native::json::<Key>(v)),
    }
}

/// The encoder of a response: the events, written whole in JSON.
fn response_encoder(head: &mut ResponseHead) -> Box<dyn Encoder<Output>> {
    let json = HeaderValue::from_static("application/json");
    head.headers.insert(CONTENT_TYPE, json);
    Box::new(All::new(|events: &[Output]| {
        serde_json::to_string(&response_of(events))
            .map_err(|e| Fault::internal("a body could not be written").with_source(e))
    }))
}

/// A response of `events`: the start's own fields, such as Cohere's `id`
/// and `meta`, the model, each document's place in order, and the usage.
fn response_of(events: &[Output]) -> Value {
    let (mut response, mut results, mut usage) = (Map::new(), Vec::new(), None);
    let mut extras = Map::new();
    for event in events {
        match event {
            Output::Start(start) => {
                merge_missing(&mut response, start.native.fields::<Key>());
                set(&mut response, "model", start.model.clone());
            }
            Output::Ranked(ranked) => {
                let mut result = json!({ "index": ranked.index, "relevance_score": ranked.score });
                if let Some(text) = &ranked.text {
                    result["document"] = json!({ "text": text });
                }
                if let Value::Object(result) = &mut result {
                    merge_missing(result, ranked.native.fields::<Key>());
                }
                results.push(result);
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
    response.insert("results".into(), results.into());
    if usage.is_some() || !extras.is_empty() {
        let mut u = Map::new();
        if let Some(usage) = usage {
            u.insert("total_tokens".into(), usage.input_tokens.into());
        }
        merge_missing(&mut u, &extras);
        response.insert("usage".into(), u.into());
    }
    response.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(input: &Input) -> Result<(String, Value), Fault> {
        let mut head = http::Request::new(()).into_parts().0;
        let mut encoder = post(&mut head, input)?;
        let mut body = Vec::new();
        encoder.encode(input, &mut body)?;
        Ok((
            head.uri.path().to_owned(),
            serde_json::from_slice(&body[0]).expect("JSON"),
        ))
    }

    fn read(answer: Value) -> Result<Vec<Output>, Fault> {
        let mut out = Vec::new();
        decode_whole(answer.to_string().into(), &mut out)?;
        Ok(out)
    }

    fn texts(texts: &[&str]) -> Vec<Document> {
        texts.iter().map(|t| Document::Text((*t).into())).collect()
    }

    #[test]
    fn a_ranking_is_a_post_of_its_query_and_documents() {
        let mut input = Input::new("capital of France?", texts(&["Paris", "Berlin"]));
        input.model = Some("rerank-v3.5".into());
        input.top_n = Some(1);
        let (path, body) = call(&input).expect("a call");
        assert_eq!(path, "/rerank");
        let sent = json!({ "model": "rerank-v3.5", "query": "capital of France?",
            "documents": ["Paris", "Berlin"], "top_n": 1 });
        assert_eq!(body, sent);
    }

    #[test]
    fn an_answer_is_each_documents_place_and_the_tokens_counted() {
        let cohere = read(json!({
            "id": "r-1",
            "results": [{ "index": 1, "relevance_score": 0.9 }, { "index": 0, "relevance_score": 0.1 }],
            "meta": { "billed_units": { "search_units": 1 } },
        }))
        .expect("read");
        let [
            Output::Start(start),
            Output::Ranked(first),
            Output::Ranked(second),
        ] = &cohere[..]
        else {
            panic!("a start and two places: {cohere:?}");
        };
        assert_eq!(start.model, None);
        assert_eq!(start.native.fields::<Key>().get("id"), Some(&json!("r-1")));
        assert_eq!((first.index, first.score), (1, 0.9));
        assert_eq!((second.index, second.score), (0, 0.1));

        let vllm = read(json!({
            "id": "rerank-1", "model": "BAAI/bge-reranker-v2-m3",
            "usage": { "prompt_tokens": 12, "total_tokens": 12 },
            "results": [{ "index": 0, "relevance_score": 0.8, "document": { "text": "Paris" } }],
        }))
        .expect("read");
        assert!(vllm.contains(&Output::Usage(Usage::new(12, 0))));
        let Some(Output::Native(more)) = vllm.last() else {
            panic!("the usage's other fields: {vllm:?}");
        };
        let more = more.value::<Key>().and_then(as_usage_extras);
        assert_eq!(more.and_then(|m| m.get("prompt_tokens")), Some(&json!(12)));
        let Output::Ranked(ranked) = &vllm[1] else {
            panic!("a place: {vllm:?}");
        };
        assert_eq!(ranked.text.as_deref(), Some("Paris"));
        assert!(ranked.native.fields::<Key>().is_empty());

        assert!(read(json!({ "results": [{ "index": 0 }] })).is_err());
        assert!(read(json!({ "data": [] })).is_err());
    }

    #[test]
    fn a_clients_request_is_read_and_sent_on_as_it_was() {
        let asked = json!({ "model": "jina-reranker-m0", "query": "q", "top_n": 2,
            "return_documents": false,
            "documents": ["a", { "text": "b" }, { "image": "https://x.test/i.png" }] });
        let input = read_request(asked.to_string().as_bytes()).expect("a request");
        assert_eq!(input.model(), Some("jina-reranker-m0"));
        assert_eq!(input.top_n, Some(2));
        assert_eq!(input.documents[..2], texts(&["a", "b"]));
        let (_, body) = call(&input).expect("a call");
        let sent = json!({ "model": "jina-reranker-m0", "query": "q", "top_n": 2,
            "return_documents": false,
            "documents": ["a", "b", { "image": "https://x.test/i.png" }] });
        assert_eq!(body, sent);

        let mut other = Input::new("q", vec![Document::Native(Native::new(json!(["t"])))]);
        other.model = Some("m".into());
        assert!(call(&other).is_err());
        assert!(read_request(br#"{"documents": ["a"]}"#).is_err());
        assert!(read_request(br#"{"query": "q", "documents": []}"#).is_err());
        assert!(read_request(br#"{"query": "q", "documents": ["a"], "top_n": -1}"#).is_err());
    }

    #[test]
    fn a_client_is_answered_each_place_in_order_and_the_usage() {
        let mut start = Start::new(Some("m".into()));
        start.native = Native::json::<Key>(json!({ "id": "r-1" }));
        let mut repeated = Ranked::new(0, 0.25);
        repeated.text = Some("a".into());
        let events = [
            Output::Start(start),
            Output::Ranked(Ranked::new(2, 0.75)),
            Output::Ranked(repeated),
            Output::Usage(Usage::new(7, 0)),
            Output::Native(Native::json::<Key>(
                json!({ "usage": { "prompt_tokens": 7 } }),
            )),
        ];
        let answer = json!({ "id": "r-1", "model": "m",
            "results": [
                { "index": 2, "relevance_score": 0.75 },
                { "index": 0, "relevance_score": 0.25, "document": { "text": "a" } },
            ],
            "usage": { "total_tokens": 7, "prompt_tokens": 7 } });
        assert_eq!(response_of(&events), answer);
    }
}
