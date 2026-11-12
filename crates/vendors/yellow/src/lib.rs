#![warn(missing_docs)]
//! The yellow vendors, one module each: maintained, but not verified end
//! to end against their live APIs. A vendor moves to green once it is.

pub mod azure;
pub mod bedrock;
pub mod cerebras;
pub mod cloudflare;
pub mod deepinfra;
pub mod deepseek;
pub mod empiriolabs;
pub mod fireworks;
pub mod groq;
pub mod minimax;
pub mod mistral;
pub mod openrouter;
pub mod perplexity;
pub mod phonic;
pub mod together;
pub mod vllm;
pub mod xai;

use respan_gateway_core::Fault;
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::content::content_type;
use respan_gateway_http::http::response::Parts as ResponseHead;
use respan_gateway_vendor_kit::{BoxFuture, Models, Vendor, VendorProvider};
use serde_json::{Map, Value};

/// Every yellow vendor.
pub const ALL: &[&Vendor] = &[
    &groq::VENDOR,
    &bedrock::VENDOR,
    &deepseek::VENDOR,
    &openrouter::VENDOR,
    &xai::VENDOR,
    &mistral::VENDOR,
    &minimax::VENDOR,
    &together::VENDOR,
    &fireworks::VENDOR,
    &azure::VENDOR,
    &vllm::VENDOR,
    &cloudflare::VENDOR,
    &cerebras::VENDOR,
    &perplexity::VENDOR,
    &empiriolabs::VENDOR,
    &deepinfra::VENDOR,
    &phonic::VENDOR,
];

/// No models, for a vendor that lists none: Bedrock's are its account's,
/// and Phonic has no list of its own.
fn no_models(_: &VendorProvider) -> BoxFuture<'_, Models> {
    Box::pin(async { Ok(Vec::new()) })
}

/// A call's JSON without its field `key`.
fn without(body: Bytes, key: &str) -> Result<Bytes, Fault> {
    let unwritten = |e| Fault::internal("a call is no JSON object").with_source(e);
    let mut call: Map<String, Value> = serde_json::from_slice(&body).map_err(unwritten)?;
    if call.shift_remove(key).is_none() {
        return Ok(body);
    }
    serde_json::to_vec(&call)
        .map(Bytes::from)
        .map_err(unwritten)
}

/// Whether an answer is a success in JSON, which a dialect may read whole.
fn json_answer(head: &ResponseHead) -> bool {
    let essence = content_type(head).and_then(|t| t.split(';').next());
    head.status.is_success()
        && essence.is_some_and(|e| e.trim().eq_ignore_ascii_case("application/json"))
}

/// A vendor's video job as OpenAI's: each of `statuses` by OpenAI's name
/// for it, and why it failed, a string, as an error's message.
fn openai_job(job: &mut Map<String, Value>, statuses: &[(&str, &str)]) {
    if let Some(status) = job.get_mut("status")
        && let Some((_, openai)) = statuses.iter().find(|(its, _)| status == its)
    {
        *status = (*openai).into();
    }
    if let Some(message) = job.get("error").and_then(Value::as_str) {
        let error = Map::from_iter([("message".to_owned(), message.into())]);
        job.insert("error".into(), error.into());
    }
}
