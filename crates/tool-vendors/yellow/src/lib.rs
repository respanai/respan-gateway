#![warn(missing_docs)]
//! The yellow tool vendors, one module each: maintained, but not verified
//! end to end against their live APIs. A vendor moves to green once it is.

pub mod deedpage;
pub mod e2b;
pub mod elevenlabs;
pub mod exa;
pub mod firecrawl;

use respan_gateway_tool_vendor_kit::ToolVendor;

/// Every yellow tool vendor.
pub const ALL: &[&ToolVendor] = &[
    &deedpage::VENDOR,
    &e2b::VENDOR,
    &elevenlabs::VENDOR,
    &exa::VENDOR,
    &firecrawl::VENDOR,
];
