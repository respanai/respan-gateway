//! The request log: each request through the gateway, as the admin API and
//! stderr show it. The HTTP edge ([`track`]) sees its head, status, timing
//! and end; a plugin ([`Recorder`]) sees its format, model, provider and
//! usage; [`attribute`] names its client.

mod count;
mod edge;
mod log;
mod record;
mod recorder;

pub use edge::track;
pub use log::{ProviderStats, RequestLog, attribute, charge};
pub use record::{RequestRecord, rfc3339};
pub use recorder::{Recorder, steps};

/// Whether `cx` is a client's request, which the HTTP edge tracked ([`track`]),
/// not one the gateway handed itself.
pub fn is_clients(cx: &respan_gateway_core::ClientContext) -> bool {
    cx.extensions().get::<log::Tracked>().is_some()
}

/// How many finished requests are kept unless the config's
/// `request_log.history` says.
pub const KEPT: usize = 1000;
