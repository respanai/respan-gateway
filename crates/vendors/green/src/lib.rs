#![warn(missing_docs)]
//! The green vendors, one module each: maintained, and verified end to end
//! against their live APIs.

pub mod anthropic;
pub mod gemini;
pub mod neon;
pub mod ollama;
pub mod openai;
pub mod respan;
pub mod typesafe;

use respan_gateway_vendor_kit::Vendor;

/// Every green vendor.
pub const ALL: &[&Vendor] = &[
    &openai::VENDOR,
    &anthropic::VENDOR,
    &gemini::VENDOR,
    &ollama::VENDOR,
    &typesafe::VENDOR,
    &respan::VENDOR,
    &neon::VENDOR,
];
