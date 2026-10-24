#![warn(missing_docs)]
//! The yellow vendors, one module each: maintained, but not verified end
//! to end against their live APIs. A vendor moves to green once it is.

pub mod azure;
pub mod bedrock;
pub mod cerebras;
pub mod cloudflare;
pub mod deepinfra;
pub mod deepseek;
pub mod fireworks;
pub mod groq;
pub mod mistral;
pub mod ollama;
pub mod openrouter;
pub mod perplexity;
pub mod together;
pub mod vllm;
pub mod xai;

use respan_gateway_vendor_kit::Vendor;

/// Every yellow vendor.
pub const ALL: &[&Vendor] = &[
    &groq::VENDOR,
    &deepseek::VENDOR,
    &openrouter::VENDOR,
    &xai::VENDOR,
    &mistral::VENDOR,
    &together::VENDOR,
    &fireworks::VENDOR,
    &ollama::VENDOR,
    &azure::VENDOR,
    &vllm::VENDOR,
    &cloudflare::VENDOR,
    &cerebras::VENDOR,
    &perplexity::VENDOR,
    &bedrock::VENDOR,
    &deepinfra::VENDOR,
];
