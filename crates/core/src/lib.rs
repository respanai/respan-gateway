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
pub use native::{FormatId, NativeFields, NativeValue};
pub use plugin::{Filter, Plugin};
pub use provider::{Provider, ProviderContext};
pub use response::Response;

/// A gateway operation that may fail with a [`Fault`].
pub type Result<T> = std::result::Result<T, Fault>;

/// The crates the kernel's contracts use, at the kernel's versions.
pub use {async_trait::async_trait, futures};
