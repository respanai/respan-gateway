#![warn(missing_docs)]
//! What plugins, vendors and hosts share beyond the core: a model's card
//! ([`Model`]), what it reads and can do, and what it costs ([`Price`]).
//!
//! A vendor fills a [`Model`] from its list; a host keeps them as its
//! [`Catalog`], its own declarations laid over its providers' lists
//! ([`Model::overlay`]), and writes them for its clients ([`Model`]
//! serializes as a `/v1/models` entry, beside its `id` and `object`); a
//! router ranks and filters by them, and a pricing plugin charges by their
//! [`Price`], each reading the same catalog.
//!
//! It is a leaf: it depends on no other crate of the gateway's.

mod catalog;
mod model;
mod price;

pub use catalog::Catalog;
pub use model::{Capability, Modality, Model, time};
pub use price::Price;

/// A request to route: what a router knows of it, and a provider's
/// [`FormatFor`] reads.
#[derive(Debug, Clone, Copy)]
pub struct Ask<'a> {
    /// Its kind, such as `chat`: the formats that read and write the same
    /// neutral types, so any of them can answer a client of another.
    pub kind: &'a str,
    /// The client's format, such as `anthropic-messages`.
    pub format: &'a str,
    /// The model it asks for, if any.
    pub model: Option<&'a str>,
    /// The client key it came with, by name, if any.
    pub client: Option<&'a str>,
    /// Whether it offers the model tools to call, or may by the time it is
    /// sent, as a tool loop adds them; a router that cannot tell says it may.
    pub tools: bool,
}

/// Which of its formats a provider is sent a request in: given the request
/// and the card of the model sent (a bare one if its list does not have
/// it), the name of one of its formats; `None` for the usual, the client's
/// own, else its first of the kind. A vendor whose formats each serve only
/// some of its models says so with one.
pub type FormatFor = fn(ask: &Ask, model: &Model) -> Option<&'static str>;
