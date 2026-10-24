#![warn(missing_docs)]
//! The vendors, in two tiers, each a crate: green
//! (`respan-gateway-vendors-green`), maintained and verified end to end
//! against their live APIs, and yellow (`respan-gateway-vendors-yellow`),
//! maintained but not verified end to end. Each
//! vendor is a module of its tier's crate, here by its name: where it is,
//! how it takes a key, and the formats it speaks, a [`Vendor`]. [`ALL`]
//! lists them. [`Vendor::provider`] makes a provider of one
//! ([`VendorProvider`]), which registers a core provider of each of its
//! formats and lists its models.
//!
//! A quirk is a vendor's deployment departing from a spec it speaks; its
//! module handles it, as data or as its dialect of the spec's format
//! (`respan_gateway_conversation_http::dialect`). What a spec's own vendor
//! does, as Gemini's signatures or OpenAI Realtime's transcription model,
//! is the spec's, in its format.

pub use respan_gateway_vendor_kit::{
    Body, BoxFuture, Capability, Connect, FORMATS, HttpTransport, KeyIn, ListModels, Modality,
    Model, Models, Network, Part, Price, Reach, Upstream, Vendor, VendorProvider, built_wrong,
    chat, format, http, json, located, models, time,
};

pub use respan_gateway_vendors_green::{anthropic, gemini, openai, respan, typesafe};
pub use respan_gateway_vendors_yellow::{
    azure, cerebras, cloudflare, cohere, deepinfra, deepseek, fireworks, groq, mistral, ollama,
    openrouter, perplexity, together, vllm, xai,
};

/// The green vendors: maintained, and verified end to end.
pub const GREEN: &[&Vendor] = respan_gateway_vendors_green::ALL;

/// The yellow vendors: maintained, not verified end to end.
pub const YELLOW: &[&Vendor] = respan_gateway_vendors_yellow::ALL;

/// Every vendor.
pub const ALL: &[&Vendor] = &[
    &openai::VENDOR,
    &anthropic::VENDOR,
    &gemini::VENDOR,
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
    &typesafe::VENDOR,
    &cloudflare::VENDOR,
    &cerebras::VENDOR,
    &perplexity::VENDOR,
    &deepinfra::VENDOR,
    &cohere::VENDOR,
    &respan::VENDOR,
];

/// The vendor `id`.
pub fn find(id: &str) -> Option<&'static Vendor> {
    ALL.iter().copied().find(|p| p.id == id)
}
