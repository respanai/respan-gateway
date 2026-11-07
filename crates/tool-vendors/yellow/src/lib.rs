#![warn(missing_docs)]
//! The yellow tool vendors, one module each: maintained, but not verified
//! end to end against their live APIs. A vendor moves to green once it is.

pub mod e2b;
pub mod elevenlabs;
pub mod exa;
pub mod firecrawl;
pub mod keenable;

use respan_gateway_tool_vendor_kit::ToolVendor;

/// Every yellow tool vendor.
pub const ALL: &[&ToolVendor] = &[
    &e2b::VENDOR,
    &elevenlabs::VENDOR,
    &exa::VENDOR,
    &firecrawl::VENDOR,
    &keenable::VENDOR,
];
