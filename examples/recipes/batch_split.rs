//! One embeddings batch split into bounded concurrent upstream calls.
//!
//! A provider takes at most a few inputs per embeddings call. The `Split`
//! plugin cuts a larger batch into chunks of at most `cap` inputs, sends them
//! at most eight at a time, retries each on a transient failure, and answers
//! the client as one response: the vectors in the client's order and the
//! usage of every call summed. A batch within the cap is sent whole.
//!
//! Each chunk is a subrequest, `cx.call(target).with_input(chunk)`, so it is a
//! call like any other: every plugin's `on_provider` runs before it, and a
//! plugin that meters calls sees each one. A subrequest built from an input
//! is not a relay, so the client's key does not go with it: the provider has
//! its own.
//!
//! Run it with `cargo run -p respan-gateway-examples --example batch_split`.
//! Five texts with a cap of two make three calls to a stand-in provider in
//! this process; the first call to arrive is refused with a 503 and retried.
//! Look for the input each call carried, and the merged body: indices 0 to 4,
//! each vector its text's letter, and usage 5.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use respan_gateway_conversation::Usage;
use respan_gateway_core::futures::{StreamExt, TryStreamExt, stream};
use respan_gateway_core::{ClientContext, Fault, Gateway, Plugin, Provider, Response, async_trait};
use respan_gateway_http::http::header::CONTENT_TYPE;
use respan_gateway_http::http::{self, HeaderValue, StatusCode};
use respan_gateway_http::http_body_util::BodyExt;
use respan_gateway_http::{Body, Sent};
use respan_gateway_http_faults::retryable;
use respan_gateway_media::embedding::{Input, Output};
use respan_gateway_media_http::openai::OpenAiEmbeddings;
use respan_gateway_plugin_fallback::{Backoff, Until, first_success};
use serde_json::{Value, json};

/// The most bytes of one message the gateway holds.
const LIMIT: usize = 32 << 20;

#[tokio::main]
async fn main() -> Result<(), Fault> {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = {
        let calls = calls.clone();
        move |req| embedder(req, calls.fetch_add(1, Ordering::SeqCst))
    };
    let gw = Gateway::new()
        .provider(Provider::new(
            "openai",
            OpenAiEmbeddings,
            respan_gateway_http::connect(provider, LIMIT),
        ))?
        .client::<Input, _, _>(OpenAiEmbeddings)?
        .plugin(Split { cap: 2 })
        .target("openai")?;

    let body = r#"{"model":"text-embedding-3-small","input":["a","b","c","d","e"]}"#;
    println!("client sends: {body}");
    let mut req = http::Request::new(Body::full(body));
    *req.method_mut() = http::Method::POST;
    *req.uri_mut() = http::Uri::from_static("/v1/embeddings");
    let (resp, _) = respan_gateway_http::handle(&gw, req, LIMIT).await;
    let status = resp.status();
    let answered = BodyExt::collect(resp.into_body()).await?.to_bytes();
    gw.idle().await;

    println!(
        "client got:   {status} {}",
        String::from_utf8_lossy(&answered)
    );
    Ok(())
}

/// A stand-in for OpenAI's `POST /embeddings`, served in this process. The
/// vector of a text is its first letter's place in the alphabet, so the merged
/// answer shows whether each vector landed at its input's index. Call 0 is
/// refused as overloaded.
async fn embedder(req: http::Request<Body>, call: usize) -> Sent {
    let body = BodyExt::collect(req.into_body()).await?.to_bytes();
    let asked: Value = serde_json::from_slice(&body).map_err(|_| Fault::bad_request("not JSON"))?;
    let texts: Vec<&str> = match &asked["input"] {
        Value::String(one) => vec![one],
        many => many
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect(),
    };
    if call == 0 {
        println!("provider call {call}: input {} -> 503", asked["input"]);
        let busy = json!({ "error": { "message": "overloaded", "type": "server_error" } });
        return Ok((reply(StatusCode::SERVICE_UNAVAILABLE, busy), None));
    }
    let data: Vec<_> = texts
        .iter()
        .enumerate()
        .map(|(i, text)| {
            let place = text.bytes().next().map_or(0, |b| b - b'a');
            json!({ "object": "embedding", "index": i, "embedding": [place] })
        })
        .collect();
    println!("provider call {call}: input {} -> 200", asked["input"]);
    let n = texts.len();
    let answer = json!({ "object": "list", "data": data, "model": "text-embedding-3-small",
        "usage": { "prompt_tokens": n, "total_tokens": n } });
    Ok((reply(StatusCode::OK, answer), None))
}

/// A JSON response.
fn reply(status: StatusCode, body: Value) -> http::Response<Body> {
    let mut resp = http::Response::new(Body::full(body.to_string()));
    *resp.status_mut() = status;
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    resp
}

/// Splits a batch over `cap` inputs into concurrent calls to the target.
struct Split {
    cap: usize,
}

#[async_trait]
impl Plugin for Split {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>, Fault> {
        let Ok(request) = cx.input::<Input>().cloned() else {
            return Ok(None);
        };
        let Some(target) = cx.target().map(str::to_owned) else {
            return Ok(None);
        };
        if request.items.len() <= self.cap {
            return Ok(None);
        }

        let calls: Vec<_> = request
            .items
            .chunks(self.cap)
            .map(|chunk| {
                let mut part = request.clone();
                part.items = chunk.to_vec();
                (chunk.len(), cx.call(target.clone()).with_input(part))
            })
            .collect();

        // Each chunk retries on its own; a chunk that still fails fails the
        // request before any merged answer is sent.
        let answers: Vec<Vec<Output>> = stream::iter(calls)
            .map(|(expected, call)| async move {
                let backoff = Backoff::new(3, Duration::from_millis(10), Duration::from_secs(1));
                let outputs: Vec<Output> = first_success([call], &backoff, Until::End, retryable)
                    .await?
                    .outputs::<Output>()
                    .try_collect()
                    .await?;
                let got = outputs
                    .iter()
                    .filter(|o| matches!(o, Output::Embedding(_)))
                    .count();
                if got != expected {
                    return Err(Fault::upstream("the provider omitted embedding vectors"));
                }
                Ok(outputs)
            })
            .buffered(8)
            .try_collect()
            .await?;

        Ok(Some(Response::stream(stream::iter(
            merge(answers, self.cap).into_iter().map(Ok),
        ))))
    }
}

/// One response from each chunk's events: the first chunk's start, every
/// vector moved to its index in the whole batch, and one usage summed.
/// Events the kind has no form for are dropped.
fn merge(answers: Vec<Vec<Output>>, cap: usize) -> Vec<Output> {
    let mut start = None;
    let mut merged = Vec::new();
    let mut usage = Usage::default();
    for (chunk, outputs) in answers.into_iter().enumerate() {
        let offset = (chunk * cap) as u32;
        for output in outputs {
            match output {
                Output::Start(s) => {
                    start.get_or_insert(Output::Start(s));
                }
                Output::Embedding(mut e) => {
                    e.index += offset;
                    merged.push(Output::Embedding(e));
                }
                Output::Usage(u) => usage += &u,
                _ => {}
            }
        }
    }
    start
        .into_iter()
        .chain(merged)
        .chain([Output::Usage(usage)])
        .collect()
}
