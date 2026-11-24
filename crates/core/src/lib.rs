#![warn(missing_docs)]
//! The respan-gateway kernel: a [`Gateway`] runs each request through its
//! [`Plugin`]s and streams the [`Response`] back (`docs/plugins.md` is the guide).
//!
//! Formats open receive futures and prepare per-event outgoing sinks.
//! [`EventStream`] implements [`futures::Stream`] and [`EventSink`] implements
//! [`futures::Sink`]; codecs and byte I/O live outside the kernel.

mod call;
mod client_output;
mod context;
mod events;
mod extensions;
mod fault;
mod format;
mod gateway;
mod input;
mod layers;
mod lifecycle;
mod live_inputs;
mod native;
mod plugin;
mod provider;
mod response;
mod values;

pub use call::Call;
pub use context::ClientContext;
pub use events::{EventSink, EventStream};
pub use extensions::Extensions;
pub use fault::{Details, ErrorCode, Fault, FaultKind, RetryAfter, guarded};
pub use format::{Connection, Format};
pub use gateway::Gateway;
pub use native::Native;
pub use plugin::{Filter, Plugin};
pub use provider::{Provider, ProviderContext};
pub use response::Response;

/// A gateway operation that may fail with a [`Fault`].
pub type Result<T> = std::result::Result<T, Fault>;

/// Per-call transport observation: whether the provider response is explicitly
/// framed as a stream. Unknown and whole responses both remain false.
#[derive(Debug, Clone, Default)]
pub struct StreamingResponse(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl StreamingResponse {
    /// Marks a response as explicitly stream-framed.
    pub fn mark_streaming(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether the transport identified a stream-framed response.
    pub fn is_streaming(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// The crates the kernel's contracts use, at the kernel's versions.
pub use {async_trait::async_trait, futures};

#[cfg(test)]
mod tests {
    use super::StreamingResponse;

    #[test]
    fn a_response_is_not_streaming_until_the_transport_says_so() {
        let shape = StreamingResponse::default();
        assert!(!shape.is_streaming());
        shape.mark_streaming();
        assert!(shape.is_streaming());
    }

    #[test]
    fn a_clone_shares_the_transport_observation() {
        let shape = StreamingResponse::default();
        let observed = shape.clone();
        shape.mark_streaming();
        assert!(observed.is_streaming());
    }
}
