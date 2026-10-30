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
pub mod mistral;
pub mod openrouter;
pub mod perplexity;
pub mod together;
pub mod vertex;
pub mod vllm;
pub mod xai;

use base64::Engine as _;
use respan_gateway_conversation::{Media, MediaSource};
use respan_gateway_core::Fault;
use respan_gateway_http::bytes::Bytes;
use respan_gateway_vendor_kit::Vendor;
use serde_json::{Map, Value};

/// Every yellow vendor.
pub const ALL: &[&Vendor] = &[
    &groq::VENDOR,
    &bedrock::VENDOR,
    &deepseek::VENDOR,
    &openrouter::VENDOR,
    &xai::VENDOR,
    &mistral::VENDOR,
    &together::VENDOR,
    &fireworks::VENDOR,
    &azure::VENDOR,
    &vllm::VENDOR,
    &cloudflare::VENDOR,
    &cerebras::VENDOR,
    &perplexity::VENDOR,
    &empiriolabs::VENDOR,
    &vertex::VENDOR,
    &vertex::EXPRESS,
    &deepinfra::VENDOR,
];

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

/// An image as a URL: its own, or a data URL of its bytes.
fn linked(media: &Media) -> String {
    match &media.source {
        MediaSource::Url(url) => url.clone(),
        MediaSource::Bytes(bytes) => format!(
            "data:{};base64,{}",
            media.mime_type,
            base64::engine::general_purpose::STANDARD.encode(bytes)
        ),
    }
}
