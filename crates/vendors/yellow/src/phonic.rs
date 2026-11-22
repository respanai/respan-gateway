//! Phonic: its speech-to-speech, as its own realtime session.

use respan_gateway_realtime_ws::PhonicSts;

use respan_gateway_vendor_kit::Vendor;

use crate::no_models;

/// Where its speech-to-speech is.
const REALTIME_URL: &str = "wss://api.phonic.ai/v1";

/// Phonic.
pub const VENDOR: Vendor = Vendor {
    id: "phonic",
    key_env: &["PHONIC_API_KEY"],
    formats: &[(&PhonicSts, Some(REALTIME_URL))],
    models: no_models,
    format_for: None,
};
