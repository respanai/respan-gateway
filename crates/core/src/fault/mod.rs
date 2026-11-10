mod extensions;

pub use extensions::{Details, ErrorCode, RetryAfter};

use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::{Extensions, Result};

pub(crate) const PANICKED: &str = "a plugin or format panicked";

/// `f`, with a panic an internal fault ([`Fault::panicked`]): a plugin's
/// or format's code is run so a panic in it fails its exchange, not the
/// gateway.
pub fn guarded<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    caught.unwrap_or_else(|_| Err(Fault::panicked()))
}

/// Who a fault is on, which decides what follows it in any protocol: who
/// is told, whether a call is tried again, whether anyone is alerted. What
/// went wrong in particular is its code ([`Fault::code`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FaultKind {
    /// The client's input or ask: never tried again.
    Client,
    /// A provider: unreachable, refusing, or broken off. Another attempt
    /// may succeed.
    Provider,
    /// The gateway: its setup, a bug, or a plugin's own refusal.
    Gateway,
    /// No one is waiting: the client left or a response was dropped. Not
    /// reported.
    Abandoned,
}

impl FaultKind {
    /// Its code, for a fault with none of its own: `bad_request`,
    /// `upstream`, `error` or `abandoned`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Client => "bad_request",
            Self::Provider => "upstream",
            Self::Gateway => "error",
            Self::Abandoned => "abandoned",
        }
    }
}

/// The one error type: a message for the client, its [`FaultKind`], a source
/// for logs, and typed extensions the edges read ([`Fault::with`]), such as an
/// HTTP status. Implement `From<YourError> for Fault` so `?` works in hooks.
#[derive(Clone)]
pub struct Fault(Arc<Inner>);

#[derive(Clone)]
struct Inner {
    message: String,
    kind: FaultKind,
    source: Option<Arc<dyn Error + Send + Sync>>,
    extensions: Extensions,
}

impl Fault {
    /// A fault with this message, safe to show a client, on the gateway.
    pub fn new(message: impl Into<String>) -> Self {
        Self::of(FaultKind::Gateway, message)
    }

    /// A fault of `kind` with this message.
    pub fn of(kind: FaultKind, message: impl Into<String>) -> Self {
        Self(Arc::new(Inner {
            message: message.into(),
            kind,
            source: None,
            extensions: Extensions::new(),
        }))
    }

    /// A fault with `e`'s display as its message and `e` as its source, so
    /// [`Fault::cause`] finds it.
    pub fn from_error(e: impl Error + Send + Sync + 'static) -> Self {
        Self::new(e.to_string()).with_source(e)
    }

    /// The underlying cause, for logs: an error or a message.
    pub fn with_source(mut self, source: impl Into<Box<dyn Error + Send + Sync>>) -> Self {
        Arc::make_mut(&mut self.0).source = Some(Arc::from(source.into()));
        self
    }

    /// An extension for the edges, such as the status a transport writes. A value
    /// of the same type replaces the last.
    pub fn with<T: Clone + Send + Sync + 'static>(mut self, extension: T) -> Self {
        Arc::make_mut(&mut self.0).extensions.insert(extension);
        self
    }

    /// The extension of type `T`, if any.
    pub fn get<T: 'static>(&self) -> Option<&T> {
        self.0.extensions.get()
    }

    /// Its kind.
    pub fn kind(&self) -> FaultKind {
        self.0.kind
    }

    /// Whether it is of `kind`.
    pub fn is(&self, kind: FaultKind) -> bool {
        self.0.kind == kind
    }

    /// The first error of type `E` in the source chain, if any
    /// ([`Fault::from_error`]).
    pub fn cause<E: Error + 'static>(&self) -> Option<&E> {
        let mut next = Error::source(self);
        while let Some(e) = next {
            if let Some(found) = e.downcast_ref() {
                return Some(found);
            }
            next = e.source();
        }
        None
    }

    /// What went wrong, for the client: its [`ErrorCode`], else its kind's
    /// ([`FaultKind::as_str`]).
    pub fn code(&self) -> &str {
        match self.get() {
            Some(ErrorCode(code)) => code,
            None => self.kind().as_str(),
        }
    }

    /// The message for the client.
    pub fn message(&self) -> &str {
        &self.0.message
    }

    /// The client's input is not one its format or kind can take.
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::of(FaultKind::Client, message)
    }

    /// Nothing takes the request: the client's, coded `not_found`.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::of(FaultKind::Client, message).coded("not_found")
    }

    /// A provider failed.
    pub fn upstream(message: impl Into<String>) -> Self {
        Self::of(FaultKind::Provider, message)
    }

    /// A provider's stream stopped before its end, coded `truncated`.
    pub fn truncated() -> Self {
        Self::of(FaultKind::Provider, "stream ended before completion").coded("truncated")
    }

    /// No one waits for it, such as a response whose client left.
    pub fn abandoned() -> Self {
        Self::of(FaultKind::Abandoned, "abandoned")
    }

    /// A setup that cannot work, coded `config`.
    pub fn config(message: impl Into<String>) -> Self {
        Self::of(FaultKind::Gateway, message).coded("config")
    }

    /// A bug, coded `internal`.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::of(FaultKind::Gateway, message).coded("internal")
    }

    /// A plugin or format that panicked: [`internal`](Self::internal).
    pub fn panicked() -> Self {
        Self::internal(PANICKED)
    }

    fn coded(self, code: &'static str) -> Self {
        self.with(ErrorCode(code.into()))
    }

    /// Something asked for `T`, not of the client's kind: a bug.
    pub(crate) fn wrong_kind<T>() -> Self {
        Self::another_kind(format!("asked for {}", std::any::type_name::<T>()))
    }

    /// A type of another kind than the client's was used; `what` goes to logs.
    pub(crate) fn another_kind(what: String) -> Self {
        let message = "a type of another kind than this request's was asked for or made";
        Self::internal(message).with_source(what)
    }
}

impl fmt::Debug for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Fault")
            .field("message", &self.0.message)
            .field("kind", &self.0.kind)
            .field("extensions", &self.0.extensions)
            .field("source", &self.0.source.as_ref().map(|s| s.to_string()))
            .finish()
    }
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.message)
    }
}

/// What cannot fail never makes one.
impl From<std::convert::Infallible> for Fault {
    fn from(never: std::convert::Infallible) -> Self {
        match never {}
    }
}

impl Error for Fault {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.0
            .source
            .as_deref()
            .map(|e| e as &(dyn Error + 'static))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct PluginError {
        limit: u32,
    }

    /// An extension an edge reads: the limit a client is told.
    #[derive(Clone, Debug, PartialEq)]
    struct Limit(u32);

    impl fmt::Display for PluginError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "over the limit of {}", self.limit)
        }
    }

    impl Error for PluginError {}

    impl From<PluginError> for Fault {
        fn from(e: PluginError) -> Self {
            Fault::new(e.to_string())
                .with(Limit(e.limit))
                .with_source(e)
        }
    }

    fn check() -> Result<()> {
        Err(PluginError { limit: 3 })?
    }

    #[test]
    fn plugin_errors_convert_with_question_mark() {
        let f = check().err();
        assert_eq!(f.as_ref().map(Fault::message), Some("over the limit of 3"));
        assert_eq!(f.as_ref().and_then(Fault::get::<Limit>), Some(&Limit(3)));
        assert!(f.as_ref().and_then(|f| f.source()).is_some());
    }

    #[test]
    fn an_extension_is_read_by_its_type() {
        let f = Fault::new("x").with(7_u8).with("s");
        assert_eq!((f.get::<u8>(), f.get::<&str>()), (Some(&7), Some(&"s")));
        assert_eq!(f.get::<u16>(), None);
        let g = f.clone().with(8_u8);
        assert_eq!((f.get::<u8>(), g.get::<u8>()), (Some(&7), Some(&8)));
    }

    #[test]
    fn a_source_error_is_found_by_its_type() {
        let f = Fault::from_error(PluginError { limit: 5 });
        assert_eq!(f.message(), "over the limit of 5");
        assert_eq!(f.cause::<PluginError>().map(|e| e.limit), Some(5));
        assert!(f.get::<Limit>().is_none());
        let wrapped = Fault::new("outer").with_source(f);
        assert_eq!(wrapped.cause::<PluginError>().map(|e| e.limit), Some(5));
    }

    #[test]
    fn each_constructor_makes_its_kind_and_code() {
        for (fault, kind, code) in [
            (Fault::bad_request("x"), FaultKind::Client, "bad_request"),
            (Fault::not_found("x"), FaultKind::Client, "not_found"),
            (Fault::upstream("x"), FaultKind::Provider, "upstream"),
            (Fault::truncated(), FaultKind::Provider, "truncated"),
            (Fault::config("x"), FaultKind::Gateway, "config"),
            (Fault::internal("x"), FaultKind::Gateway, "internal"),
            (Fault::new("x"), FaultKind::Gateway, "error"),
            (Fault::abandoned(), FaultKind::Abandoned, "abandoned"),
        ] {
            assert_eq!((fault.kind(), fault.code()), (kind, code));
        }
        let coded = Fault::internal("x").with(ErrorCode("mine".into()));
        assert_eq!(coded.code(), "mine");
    }
}
