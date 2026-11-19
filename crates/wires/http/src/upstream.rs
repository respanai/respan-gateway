//! A provider format as its spec has it reached ([`Upstream`]): where it
//! takes a provider's key, whether over HTTP or as a WebSocket session,
//! what redirects it follows, and whether its paths are whole. A vendor
//! lists its formats by it, each at a base URL of the vendor's; what the
//! spec says is the format's, not the vendor's.

use std::sync::Arc;

use http::Uri;
use respan_gateway_core::{Extensions, Provider};

use crate::ProviderIo;

/// What makes a provider's calls, one I/O each.
pub type Connect = Arc<dyn Fn(Extensions) -> ProviderIo + Send + Sync>;

/// Where a format takes a provider's key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyIn {
    /// `Authorization: Bearer <key>`.
    Bearer,
    /// The key alone, in this header (lower case).
    Header(&'static str),
}

/// How a format's calls reach their provider.
#[derive(Debug, Clone, Copy)]
pub enum Reach {
    /// Over HTTP, following the redirects `follow` allows, if any: a call
    /// that follows one waits at most 10 s to connect.
    Http {
        /// The redirects followed.
        follow: Option<fn(&Uri) -> bool>,
    },
    /// As a WebSocket session, its base URL in `ws`/`wss`.
    Session {
        /// Whether the provider's binary frames are text, as Gemini Live
        /// sends its JSON.
        binary_text: bool,
    },
}

/// A provider format as its spec has it reached. What it does not say is
/// the common case: its key a bearer token, over HTTP, its paths beside its
/// base URL.
pub trait Upstream: Send + Sync + 'static {
    /// Its name, as a config names it.
    fn name(&self) -> &str;

    /// Where it takes a provider's key.
    fn key(&self) -> KeyIn {
        KeyIn::Bearer
    }

    /// How its calls reach the provider.
    fn reach(&self) -> Reach {
        Reach::Http { follow: None }
    }

    /// Whether its paths are whole, as the passthrough's (the client's own)
    /// and Gemini Live's are: it takes its base URL's origin alone.
    fn at_origin(&self) -> bool {
        false
    }

    /// The core provider `name` of it, its calls made by `connect`.
    fn provider(&self, name: String, connect: Connect) -> Provider;
}

impl std::fmt::Debug for dyn Upstream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}
