use respan_gateway_core::{ErrorCode, Fault, FaultKind};
use respan_gateway_http::http::StatusCode;
use respan_gateway_http_faults::Status;

/// What a realtime format cannot translate, a [`Fault`] by `?` or
/// `Fault::from`, which keeps it as its source ([`Fault::cause`]).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A client's event the provider's `format` cannot write: the client's,
    /// coded `unsupported`, so its session closes 1008.
    #[error("{what} cannot be translated to {format}")]
    Untranslatable {
        /// The format that cannot write it.
        format: &'static str,
        /// What it is.
        what: String,
    },
    /// A provider's event the client's `format` cannot write: the
    /// gateway's, a 502 coded `untranslatable`.
    #[error("{what} cannot be translated to {format}")]
    Unwritable {
        /// The format that cannot write it.
        format: &'static str,
        /// What it is.
        what: String,
    },
}

impl From<Error> for Fault {
    fn from(e: Error) -> Self {
        let fault = match e {
            Error::Untranslatable { .. } => {
                Fault::of(FaultKind::Client, e.to_string()).with(ErrorCode("unsupported".into()))
            }
            Error::Unwritable { .. } => Fault::new(e.to_string())
                .with(ErrorCode("untranslatable".into()))
                .with(Status(StatusCode::BAD_GATEWAY)),
        };
        fault.with_source(e)
    }
}
