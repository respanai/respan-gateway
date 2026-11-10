use respan_gateway_core::{ErrorCode, Fault, FaultKind};
use respan_gateway_http::http::StatusCode;
use respan_gateway_http_faults::Status;

/// What a format cannot translate, a [`Fault`] by `?` or `Fault::from`,
/// which keeps it as its source ([`Fault::cause`]).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A request's part the provider's format has no form for: the
    /// client's, coded `unsupported`.
    #[error("{0} cannot be translated to another format yet")]
    Untranslatable(&'static str),
    /// A response's content the client's format cannot write anew, such as
    /// a tool call: a 502 coded `untranslatable`.
    #[error("the response holds content that cannot yet be written anew in this format")]
    Unwritable,
    /// A provider's finish reason the client's format has no name for, as
    /// [`Error::Unwritable`].
    #[error(
        "the response holds content that cannot yet be written anew in this format: \
         {format}'s finish reason `{reason}`"
    )]
    UnwritableFinish {
        /// The provider's format.
        format: String,
        /// Its finish reason.
        reason: String,
    },
}

impl From<Error> for Fault {
    fn from(e: Error) -> Self {
        let fault = match e {
            Error::Untranslatable(_) => {
                Fault::of(FaultKind::Client, e.to_string()).with(ErrorCode("unsupported".into()))
            }
            _ => Fault::new(e.to_string())
                .with(ErrorCode("untranslatable".into()))
                .with(Status(StatusCode::BAD_GATEWAY)),
        };
        fault.with_source(e)
    }
}
