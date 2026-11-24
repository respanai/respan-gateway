#![warn(missing_docs)]
//! The yellow vendors, one module each: maintained, but not verified end
//! to end against their live APIs. A vendor moves to green once it is.

pub mod azure;
pub mod baseten;
pub mod bedrock;
pub mod cerebras;
pub mod cloudflare;
pub mod concentrate;
pub mod deepinfra;
pub mod deeprelay;
pub mod deepseek;
pub mod empiriolabs;
pub mod fireworks;
pub mod groq;
pub mod minimax;
pub mod mistral;
pub mod morph;
pub mod opencode;
pub mod openrouter;
pub mod perplexity;
pub mod phonic;
pub mod primeintellect;
pub mod qwen;
pub mod stepfun;
pub mod together;
pub mod vllm;
pub mod xai;
pub mod zai;

use respan_gateway_conversation::{Media, MediaSource};
use respan_gateway_conversation_http::fields::media_url;
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
    &qwen::VENDOR,
    &morph::VENDOR,
    &stepfun::VENDOR,
    &baseten::VENDOR,
    &concentrate::VENDOR,
    &primeintellect::VENDOR,
    &zai::VENDOR,
    &deeprelay::VENDOR,
    &opencode::VENDOR,
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

/// `images` as URLs, each of bytes a data URL of them, so that an edit is
/// sent as JSON rather than as a form.
fn as_urls(images: &mut [Media]) {
    for media in images {
        if matches!(media.source, MediaSource::Bytes(_)) {
            media.source = MediaSource::Url(media_url(media));
        }
    }
}

/// Whether an answer is a success in JSON, which a dialect may read whole.
fn json_answer(head: &ResponseHead) -> bool {
    let essence = content_type(head).and_then(|t| t.split(';').next());
    head.status.is_success()
        && essence.is_some_and(|e| e.trim().eq_ignore_ascii_case("application/json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vendor_with_no_formats_given_serves_decisions_at_its_own_base() {
        let at = |vendor: &Vendor| vendor.format("decisions").map(|(_, url)| url);
        assert_eq!(
            at(&deepinfra::VENDOR),
            Some(Some("https://api.deepinfra.com/v1"))
        );
        assert_eq!(
            at(&perplexity::VENDOR),
            Some(Some("https://api.perplexity.ai/v1"))
        );
    }
}
