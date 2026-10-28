#![warn(missing_docs)]
#![doc = include_str!("../README.md")]
//! HTTP at the gateway's edges: its formats and its rules.
//!
//! Formats implement the kernel's `Format` directly over [`ClientIo`] and
//! [`ProviderIo`]: a format gives each edge an [`Encoder`] of the values it
//! writes and a [`Decoder`] of those it reads, and the binding drives them
//! over the body.
//!
//! Its rules live at the four places that own them:
//!
//! - **The adapter** ([`handle`], [`ClientIo::accept`]) strips the
//!   connection's headers from a request, lifts the client's credentials
//!   into a [`Credential`], publishes the request as it came ([`Original`](crate::raw::Original)),
//!   writes what no format can in plain text, and strips what a hop must not
//!   carry from the response.
//! - **The bindings** ([`ClientIo::bind`], [`ProviderIo::bind`]) write no
//!   body where HTTP has none ([`bodiless`](crate::rules::bodiless)), write a fault's status and
//!   `Retry-After`, read a refused answer whole into a fault, and strip
//!   from every call what a proxy must not pass on, whatever its transport.
//! - **An outbound transport** can bound its provider head and polled body
//!   frame gaps with [`watchdog`](crate::call::watchdog). HTTP retry classification is explicitly
//!   selected by a routing caller through [`retryable`](respan_gateway_http_faults::retryable); it is not built into
//!   routing's timing-only backoff.
//! - **A transport** decides whose keys reach the provider ([`credentials`](crate::call::credentials),
//!   [`CallHeaders`](crate::call::CallHeaders)), joins its path to its base URL ([`base_url`](crate::call::base_url),
//!   [`prepare_call`](crate::call::prepare_call)), and strips a provider origin's own headers from its
//!   response ([`strip_origin_headers`](crate::call::strip_origin_headers)).
//!
//! Its modules:
//!
//! - `edge`: how HTTP meets the kernel, [`ClientIo`] and [`handle`] for a
//!   server, [`ProviderIo`] and [`connect`] for a transport, and [`Body`].
//! - `codec`: [`Encoder`] and [`Decoder`]; [`Sse`] and [`Whole`], which
//!   frame what a decoder reads, and [`All`], what an encoder writes.
//! - [`rules`]: HTTP's semantics: what succeeds,
//!   [`bodiless`](crate::rules::bodiless), and the headers a hop must not carry.
//!   A fault's status and retry are `respan-gateway-http-faults`'.
//! - [`call`]: an outbound call, its URL, keys and headers ([`prepare_call`](crate::call::prepare_call),
//!   [`credentials`](crate::call::credentials)) and its [`watchdog`](crate::call::watchdog).
//! - [`raw`]: the client's request and the provider's answer as they came
//!   ([`Original`](crate::raw::Original), [`RawReply`](crate::raw::RawReply)), and the mark of a call relayed to a provider of the client's own format ([`Relayed`](crate::raw::Relayed)), which `respan-gateway-plugin-relay`'s `Relay` sets.
//! - [`path`]: how a request's path is matched ([`clean_path`](crate::path::clean_path),
//!   [`path_ends_with`](crate::path::path_ends_with)) and forwarded
//!   ([`forwarded_path`](crate::path::forwarded_path), [`set_path`](crate::path::set_path)).
//! - [`content`]: a body's content type, whether a response is a stream,
//!   and the JSON of a body or event.
//! - [`upstream`]: a provider format as its spec has it reached
//!   ([`Upstream`]), for a vendor to list.
//!
//! Hosting a gateway is `respan-gateway-serve`'s, not a wire's.

extern crate self as respan_gateway_http;

pub mod content;

pub mod call;
mod codec;
mod credential;
pub mod dialect;
mod edge;
pub mod path;
pub mod raw;
pub mod rules;
pub mod upstream;

/// The crates HTTP formats and transports use, at these versions.
pub use {bytes, http, http_body, http_body_util};

pub use codec::{All, Decoder, Encoder, Sse, Whole};
pub use credential::Credential;
pub use edge::{
    Body, ClientIo, Completion, Frames, ProviderIo, Reply, Rewrite, Sent, connect, handle,
    handle_live,
};
pub use upstream::{Connect, KeyIn, Reach, Upstream};
