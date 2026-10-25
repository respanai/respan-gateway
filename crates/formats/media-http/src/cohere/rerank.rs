//! Cohere Rerank: `POST /rerank` (JSON), answered whole in JSON.

use crate::edge::{bind_provider, sends};
use crate::openai::{
    Form, as_usage_extras, bind_client, content_type, foreign, invalid, json_bytes, missing,
    post_to, take_count, take_string, unreadable, usage_extras, whole_json,
};
use respan_gateway_conversation::Usage;
use respan_gateway_conversation_http::fields::{kept, merge_missing, native_of, rest_of, set};
use respan_gateway_core::{Connection, Fault, Format, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::{self, Method, request::Parts as RequestHead};
use respan_gateway_http::path::path_ends_with;
use respan_gateway_http::{All, ClientIo, Decoder, Encoder, ProviderIo, Whole};
use respan_gateway_http::{Connect, Upstream};
use respan_gateway_media::rerank::{Input, Output, Ranked, Start};
use serde_json::{Map, Value};

const NAME: &str = "cohere-rerank";
const RERANK: &str = "/rerank";

/// Cohere Rerank, as Cohere and compatible vendors serve it: a `POST` to a
/// path ending in `/rerank` (its SDKs' `/v1/rerank` and `/v2/rerank`
/// alike), with a `query` and `documents`, answered with `results` of an
/// `index` and a `relevance_score`.
///
/// A document is a string, or an object with a `text`, as v1 and some
/// vendors take it; it is relayed as a string. `top_n` and
/// `return_documents` are relayed as the client wrote them. A result's
/// `document` is a string or an object with a `text`, and is written as
/// the latter; results listed as `data`, as Fireworks lists them, are read
/// as `results`.
///
/// What a provider reports it used is read from wherever it puts it: tokens
/// from `usage` (`prompt_tokens`, else `total_tokens`, and
/// `completion_tokens`), else Cohere's `meta.tokens`, else the tokens it
/// bills (`meta.billed_units`, as for its Rerank 5); search units from
/// `meta.billed_units.search_units`, else OpenRouter's
/// `usage.search_units`. A response is written with the tokens in both
/// `usage` and `meta.tokens`, and the units in both `usage` and
/// `meta.billed_units`, so that clients of either read them. Its errors are
/// written in OpenAI's schema.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CohereRerank;

impl CohereRerank {
    /// The format's name, `cohere-rerank`, which tags its native fields.
    pub const NAME: &'static str = NAME;
}

impl Format<ClientIo, Input, Output> for CohereRerank {
    fn name(&self) -> &str {
        NAME
    }

    fn matches(&self, io: &ClientIo) -> bool {
        let request = io.request();
        request.method() == Method::POST && path_ends_with(request.uri(), RERANK)
    }

    fn bind(&self, io: ClientIo) -> Result<Connection<Input, Output>, Fault> {
        Ok(bind_client(io, decode_request, response_encoder))
    }
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
        Ok(bind_provider(io, NAME, post, |_, _| Some(read_response())))
    }
}

/// A client's request.
fn decode_request(_: &RequestHead, form: &Form) -> Result<Input, Fault> {
    let fields = form.fields()?;
    let mut map = fields.map;
    if let Some((name, _)) = fields.files.first() {
        return Err(invalid(name, "is not a file this endpoint takes"));
    }
    let query = take_string(&mut map, "query")?.ok_or_else(|| missing("query"))?;
    let documents = map
        .remove("documents")
        .ok_or_else(|| missing("documents"))?;
    let mut input = Input::new(query, read_documents(&documents)?);
    input.model = take_string(&mut map, "model")?;
    input.top_n = take_count(&mut map, "top_n")?;
    input.return_documents = match map.remove("return_documents") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(b)) => Some(b),
        Some(_) => return Err(invalid("return_documents", "must be a boolean")),
    };
    input.native = native_of(NAME, map);
    Ok(input)
}

/// The texts of a request's `documents`, each a string or an object with
/// a `text`.
fn read_documents(v: &Value) -> Result<Vec<String>, Fault> {
    let bad = || {
        invalid(
            "documents",
            "must be an array of strings or {\"text\"} objects",
        )
    };
    let all = v.as_array().ok_or_else(bad)?;
    if all.is_empty() {
        return Err(invalid("documents", "must not be empty"));
    }
    all.iter()
        .map(|d| text_of(d).map(str::to_owned))
        .collect::<Option<_>>()
        .ok_or_else(bad)
}

/// A document's text: a string, or an object's `text`.
fn text_of(v: &Value) -> Option<&str> {
    v.as_str().or_else(|| v.get("text")?.as_str())
}

/// The call of `input`, in JSON.
fn post(head: &mut RequestHead, input: &Input) -> Result<Box<dyn Encoder<Input>>, Fault> {
    post_to(head, RERANK, "application/json")?;
    Ok(sends(request_body(input)?))
}

/// The JSON body of a call of `input`.
fn request_body(input: &Input) -> Result<Bytes, Fault> {
    let mut v = Map::new();
    set(&mut v, "model", input.model.clone());
    v.insert("query".into(), input.query.clone().into());
    v.insert("documents".into(), input.documents.clone().into());
    set(&mut v, "top_n", input.top_n);
    set(&mut v, "return_documents", input.return_documents);
    merge_missing(&mut v, &kept(NAME, &input.native)?);
    json_bytes(&v)
}

/// The decoder of a provider's answer: one JSON body.
fn read_response() -> Box<dyn Decoder<Output>> {
    Box::new(Whole::new(decode_whole))
}

/// A whole number `v`, written as an integer or as a float without a
/// fraction, as Cohere writes its billed units.
fn whole(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| {
        let n = v.as_f64()?;
        (n >= 0.0 && n.fract() == 0.0 && n <= u64::MAX as f64).then_some(n as u64)
    })
}

/// A count in `v` at `key`: 0 if none, an error if not a whole number.
fn count(v: &Value, key: &str) -> Result<u64, Fault> {
    match &v[key] {
        Value::Null => Ok(0),
        n => whole(n)
            .ok_or_else(|| Fault::upstream(format!("a usage count is not a whole number: {n}"))),
    }
}

/// A response's ranked documents: Cohere's `results`, or `data`, as
/// Fireworks and Voyage list them.
fn results_of(v: &Value) -> Option<&Vec<Value>> {
    v["results"].as_array().or_else(|| v["data"].as_array())
}

/// The events of a whole JSON response.
fn decode_whole(raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
    let v = whole_json(NAME, raw, |v| results_of(v).is_some())?;
    let mut start = Start::new(v["model"].as_str().map(Into::into));
    start.id = v["id"].as_str().map(Into::into);
    let mut rest = rest_of(&v, &["id", "model", "results", "data", "usage", "meta"]);
    let mut meta = v["meta"].as_object().cloned().unwrap_or_default();
    let tokens = meta.remove("tokens").unwrap_or_default();
    let mut billed = meta
        .remove("billed_units")
        .and_then(|b| b.as_object().cloned())
        .unwrap_or_default();
    let searches = billed.remove("search_units");
    let billed = Value::Object(billed);
    if billed.as_object().is_some_and(|b| !b.is_empty()) {
        meta.insert("billed_units".into(), billed.clone());
    }
    if !meta.is_empty() {
        rest.insert("meta".into(), meta.into());
    }
    start.native = native_of(NAME, rest);
    out.push(Output::Start(start));
    for (at, item) in results_of(&v).into_iter().flatten().enumerate() {
        let index = item["index"].as_u64().and_then(|i| u32::try_from(i).ok());
        let score = item["relevance_score"]
            .as_f64()
            .ok_or_else(|| unreadable(NAME, "holds a result with no relevance_score"))?;
        let mut ranked = Ranked::new(index.unwrap_or(at as u32), score);
        ranked.document = text_of(&item["document"]).map(str::to_owned);
        ranked.native = native_of(
            NAME,
            rest_of(item, &["index", "relevance_score", "document"]),
        );
        out.push(Output::Result(ranked));
    }
    // Tokens from `usage`, else Cohere's `meta.tokens`, else the tokens it
    // bills a model billed by the token, as its Rerank 5.
    let used = &v["usage"];
    let counted = |key| !used[key].is_null();
    let usage = if counted("prompt_tokens") || counted("total_tokens") {
        let input = match counted("prompt_tokens") {
            true => count(used, "prompt_tokens")?,
            false => count(used, "total_tokens")?,
        };
        Usage::new(input, count(used, "completion_tokens")?)
    } else if tokens.is_object() {
        Usage::new(
            count(&tokens, "input_tokens")?,
            count(&tokens, "output_tokens")?,
        )
    } else {
        Usage::new(
            count(&billed, "input_tokens")?,
            count(&billed, "output_tokens")?,
        )
    };
    if !usage.is_empty() {
        out.push(Output::Usage(usage));
    }
    if used.is_object() {
        let extras = rest_of(
            used,
            &[
                "prompt_tokens",
                "completion_tokens",
                "total_tokens",
                "search_units",
            ],
        );
        out.extend(usage_extras(NAME, extras).map(Output::Native));
    }
    // Search units as Cohere bills them, else as OpenRouter reports them.
    if let Some(units) = searches.as_ref().or(used.get("search_units")) {
        let units = whole(units).ok_or_else(|| {
            Fault::upstream(format!("search_units is not a whole number: {units}"))
        })?;
        out.push(Output::Searches(units));
    }
    Ok(())
}

/// The encoder of a response to a client's request.
fn response_encoder(
    _request: &http::Request<Bytes>,
    _asked: &Map<String, Value>,
    head: &mut http::response::Parts,
) -> Box<dyn Encoder<Output>> {
    content_type(head, "application/json");
    Box::new(All::new(encode_whole))
}

/// A whole response of `events`.
fn encode_whole(events: &[Output]) -> Result<Bytes, Fault> {
    let (mut start, mut results, mut usage, mut searches) = (None, Vec::new(), None, None);
    let mut extras = Map::new();
    for event in events {
        match event {
            Output::Start(s) => start = Some(s),
            Output::Result(r) => {
                let mut item = Map::new();
                item.insert("index".into(), r.index.into());
                item.insert("relevance_score".into(), r.relevance_score.into());
                if let Some(text) = &r.document {
                    item.insert("document".into(), serde_json::json!({ "text": text }));
                }
                merge_missing(&mut item, &kept(NAME, &r.native)?);
                results.push(Value::Object(item));
            }
            Output::Usage(u) => *usage.get_or_insert_with(Usage::default) += u,
            Output::Searches(n) => *searches.get_or_insert(0u64) += n,
            Output::Native(n) if &*n.format != NAME => return Err(foreign(NAME, n)),
            // A body has no place for another event.
            Output::Native(n) => {
                if let Some(more) = as_usage_extras(&n.value) {
                    merge_missing(&mut extras, more);
                }
            }
            _ => {}
        }
    }
    let mut body = Map::new();
    if let Some(start) = start {
        set(&mut body, "id", start.id.clone());
        set(&mut body, "model", start.model.clone());
    }
    body.insert("results".into(), results.into());
    // Tokens as `usage` and Cohere's `meta.tokens`, search units as
    // OpenRouter's `usage` and Cohere's `meta.billed_units`.
    let (mut used, mut meta) = (Map::new(), Map::new());
    if let Some(usage) = usage {
        used.insert("prompt_tokens".into(), usage.input_tokens.into());
        used.insert("completion_tokens".into(), usage.output_tokens.into());
        let total = usage.input_tokens.saturating_add(usage.output_tokens);
        used.insert("total_tokens".into(), total.into());
        let mut tokens = Map::new();
        tokens.insert("input_tokens".into(), usage.input_tokens.into());
        tokens.insert("output_tokens".into(), usage.output_tokens.into());
        meta.insert("tokens".into(), tokens.into());
    }
    if let Some(units) = searches {
        used.insert("search_units".into(), units.into());
        let mut billed = Map::new();
        billed.insert("search_units".into(), units.into());
        meta.insert("billed_units".into(), billed.into());
    }
    merge_missing(&mut used, &extras);
    if !used.is_empty() {
        body.insert("usage".into(), used.into());
    }
    if !meta.is_empty() {
        body.insert("meta".into(), meta.into());
    }
    if let Some(start) = start {
        merge_missing(&mut body, &kept(NAME, &start.native)?);
    }
    json_bytes(&body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_core::FaultKind;
    use serde_json::json;

    fn decoded(raw: Value) -> Vec<Output> {
        let mut events = Vec::new();
        decode_whole(Bytes::from(raw.to_string()), &mut events).expect("events");
        events
    }

    #[test]
    fn documents_are_strings_or_text_objects() {
        let read = read_documents(&json!(["a", {"text": "b", "id": 1}])).expect("documents");
        assert_eq!(read, ["a", "b"]);
        for bad in [json!([]), json!("a"), json!([1]), json!([{"title": "x"}])] {
            let refused = read_documents(&bad);
            assert!(refused.is_err_and(|f| f.is(FaultKind::Client)), "{bad}");
        }
    }

    #[test]
    fn a_request_is_relayed_with_its_extras() {
        let asked = json!({
            "model": "m", "query": "q", "documents": ["a", {"text": "b"}],
            "top_n": 2, "return_documents": true, "max_tokens_per_doc": 9,
        });
        let form = Form {
            headers: &Default::default(),
            raw: &Bytes::from(asked.to_string()),
            original: None,
        };
        let (head, _) = http::Request::new(()).into_parts();
        let input = decode_request(&head, &form).expect("input");
        assert_eq!(input.query, "q");
        assert_eq!(input.documents, ["a", "b"]);
        assert_eq!((input.top_n, input.return_documents), (Some(2), Some(true)));
        let sent: Value =
            serde_json::from_slice(&request_body(&input).expect("body")).unwrap_or_default();
        assert_eq!(
            sent,
            json!({
                "model": "m", "query": "q", "documents": ["a", "b"],
                "top_n": 2, "return_documents": true, "max_tokens_per_doc": 9,
            })
        );
    }

    #[test]
    fn a_cohere_response_gives_scores_search_units_and_tokens() {
        let events = decoded(json!({
            "id": "r1",
            "results": [{"index": 1, "relevance_score": 0.9}, {"index": 0, "relevance_score": 0.1}],
            "meta": {"api_version": {"version": "2"},
                "billed_units": {"search_units": 1}, "tokens": {"input_tokens": 12, "output_tokens": 0}},
        }));
        let scores: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Output::Result(r) => Some((r.index, r.relevance_score)),
                _ => None,
            })
            .collect();
        assert_eq!(scores, [(1, 0.9), (0, 0.1)]);
        assert!(events.contains(&Output::Searches(1)));
        assert!(events.contains(&Output::Usage(Usage::new(12, 0))));
        let Some(Output::Start(start)) = events.first() else {
            panic!("no start");
        };
        assert_eq!(start.id.as_deref(), Some("r1"));
    }

    #[test]
    fn a_response_with_usage_gives_documents_and_total_tokens() {
        let events = decoded(json!({
            "model": "reranker-v2",
            "usage": {"total_tokens": 31},
            "results": [{"index": 0, "document": {"text": "a"}, "relevance_score": 0.5}],
        }));
        assert!(events.contains(&Output::Usage(Usage::new(31, 0))));
        let Some(Output::Result(ranked)) = events.get(1) else {
            panic!("no result");
        };
        assert_eq!(ranked.document.as_deref(), Some("a"));
        assert!(!events.iter().any(|e| matches!(e, Output::Searches(_))));
    }

    #[test]
    fn a_response_is_written_with_its_usage_and_extras() {
        let raw = json!({
            "id": "r1",
            "model": "m",
            "results": [{"index": 0, "relevance_score": 0.5, "document": {"text": "a"}, "x": 1}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 0, "total_tokens": 3, "search_units": 1, "y": 2},
            "meta": {"api_version": {"version": "2"}, "billed_units": {"search_units": 1},
                "tokens": {"input_tokens": 3, "output_tokens": 0}},
            "w": true,
        });
        let written = encode_whole(&decoded(raw.clone())).expect("a body");
        let written: Value = serde_json::from_slice(&written).unwrap_or_default();
        assert_eq!(written, raw);
    }

    #[test]
    fn a_model_billed_by_the_token_gives_its_billed_tokens() {
        // Cohere's Rerank 5 bills tokens, and says no `meta.tokens`.
        let events = decoded(json!({
            "id": "r1", "results": [{"index": 0, "relevance_score": 0.7}],
            "meta": {"api_version": {"version": "2"}, "billed_units": {"input_tokens": 16.0}},
        }));
        assert!(events.contains(&Output::Usage(Usage::new(16, 0))));
        assert!(!events.iter().any(|e| matches!(e, Output::Searches(_))));
    }

    #[test]
    fn results_listed_as_data_with_text_documents_are_read() {
        // Fireworks': `data`, each document its text.
        let events = decoded(json!({
            "object": "list", "model": "accounts/fireworks/models/qwen3-reranker-8b",
            "data": [{"index": 1, "relevance_score": 0.97, "document": "b"}],
            "usage": {"prompt_tokens": 42, "total_tokens": 42},
        }));
        let Some(Output::Result(ranked)) = events.get(1) else {
            panic!("no result");
        };
        assert_eq!((ranked.index, ranked.document.as_deref()), (1, Some("b")));
        assert!(events.contains(&Output::Usage(Usage::new(42, 0))));
    }

    #[test]
    fn search_units_in_usage_are_read_and_its_cost_kept() {
        // OpenRouter's: units and cost in `usage`.
        let raw = json!({
            "model": "cohere/rerank-v3.5", "results": [{"index": 0, "relevance_score": 0.8}],
            "usage": {"search_units": 1, "cost": 0.002},
        });
        let events = decoded(raw.clone());
        assert!(events.contains(&Output::Searches(1)));
        let written: Value =
            serde_json::from_slice(&encode_whole(&events).expect("a body")).unwrap_or_default();
        assert_eq!(written["usage"], raw["usage"]);
        assert_eq!(written["meta"]["billed_units"]["search_units"], 1);
    }

    #[test]
    fn a_result_with_no_score_is_refused() {
        let mut events = Vec::new();
        let body = json!({"results": [{"index": 0}]}).to_string();
        assert!(decode_whole(Bytes::from(body), &mut events).is_err());
    }
}
