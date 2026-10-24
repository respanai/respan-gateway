#![warn(missing_docs)]
//! A vendor as a template: how it takes a key, and the formats it speaks,
//! each at its base URL ([`Vendor`]); and a provider of it, which registers
//! a core provider of each of its formats and lists its models
//! ([`VendorProvider`]). The vendors themselves are
//! `respan-gateway-vendors`'.
//!
//! A format is a spec and names no vendor; how its spec has it reached is
//! its own ([`Upstream`]): where it takes the key, over HTTP or as a
//! session, whether its paths are whole. Where a vendor's deployment
//! departs from a spec, its vendor says so: as data, such as how it lists
//! its models ([`ListModels`]), or in the formats it speaks, such as its
//! dialect of a spec. The kit runs nothing of a vendor's beside its
//! formats. What vendors share is here too: every spec's format, as a
//! provider of no vendor speaks it ([`FORMATS`]), and what vendors of Chat
//! Completions share in their dialects ([`chat`]), and of Gemini's
//! transcriptions ([`transcription`]).

pub mod chat;
mod formats;
pub mod models;
mod provider;
pub mod transcription;

pub use formats::{FORMATS, format};
pub use models::{BoxFuture, Capability, ListModels, Modality, Model, Models, Price, time};
pub use provider::{Part, VendorProvider};
pub use respan_gateway_http::{Body, Connect, KeyIn, Reach, Upstream, http};
pub use respan_gateway_http_faults::built_wrong;
pub use respan_gateway_http_transport::{HttpTransport, Network, json, reqwest};
pub use respan_gateway_sdk::{Ask, FormatFor};

/// A vendor: a template of the providers of it ([`Vendor::provider`]). A
/// vendor says what it is and the formats it speaks, each at the base URL
/// its docs give; the rest is [`Vendor::DEFAULT`]'s:
///
/// ```ignore
/// pub const VENDOR: Vendor = Vendor {
///     id: "mistral",
///     key_env: &["MISTRAL_API_KEY"],
///     formats: &[
///         (&OpenAiChat, Some("https://api.mistral.ai/v1")),
///         (&OpenAiEmbeddings, Some("https://api.mistral.ai/v1")),
///     ],
///     ..Vendor::DEFAULT
/// };
/// ```
pub struct Vendor {
    /// Its id, as the gateway's config names it: `vendor = "<id>"`.
    pub id: &'static str,
    /// The environment variables its key is read from, the first set; none
    /// for a vendor that needs no key.
    pub key_env: &'static [&'static str],
    /// The formats it speaks, its dialects of a spec among them, each at
    /// its base URL, if it has one of its own: a `{NAME}` in it is the
    /// environment variable `NAME` ([`located`]), such as an account's id.
    pub formats: &'static [(&'static dyn Upstream, Option<&'static str>)],
    /// How it lists a provider's models: as OpenAI does (`GET /models`,
    /// [`models::openai`]) unless it says.
    pub models: ListModels,
    /// The format a request for a model goes in, if not the usual: the
    /// client's own, else its first of the kind.
    pub format_for: Option<FormatFor>,
    /// Its base URLs with their names from the environment: [`located`],
    /// unless a value it names changes more than its place, such as a
    /// location whose host is another.
    pub locate: Locate,
}

/// A vendor's base URL, each `{NAME}` in it the environment variable
/// `NAME`; `Err` with the name of one not set ([`located`]).
pub type Locate = for<'a> fn(&'a str) -> Result<String, &'a str>;

impl std::fmt::Debug for Vendor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vendor")
            .field("id", &self.id)
            .field("formats", &self.formats)
            .finish_non_exhaustive()
    }
}

impl Vendor {
    /// A vendor of no id, key or formats, listing its models as OpenAI
    /// does: what a vendor does not say.
    pub const DEFAULT: Vendor = Vendor {
        id: "",
        key_env: &[],
        formats: &[],
        models: models::openai,
        format_for: None,
        locate: located,
    };

    /// Its format `name`, and that format's base URL, if it speaks it.
    pub fn format(&self, name: &str) -> Option<(&'static dyn Upstream, Option<&'static str>)> {
        self.formats.iter().copied().find(|(f, _)| f.name() == name)
    }
}

/// `url`, each `{NAME}` in it the environment variable `NAME`; `Err` with
/// the name of one not set.
pub fn located(url: &str) -> Result<String, &str> {
    let (mut rest, mut located) = (url, String::new());
    while let Some((before, after)) = rest.split_once('{') {
        let (name, after) = after.split_once('}').unwrap_or((after, ""));
        let value = std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        located += before;
        located += value.as_deref().ok_or(name)?.trim();
        rest = after;
    }
    Ok(located + rest)
}
