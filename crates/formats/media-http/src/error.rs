use respan_gateway_core::{ErrorCode, Fault, FaultKind};
use respan_gateway_http::http::StatusCode;
use respan_gateway_http_faults::Status;

/// What a media format cannot do, a [`Fault`] by `?` or `Fault::from`,
/// which keeps it as its source ([`Fault::cause`]).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// An ask the provider never takes, as it says: the client's, coded
    /// `unsupported`.
    #[error("{0}")]
    Unsupported(&'static str),
    /// A video's status the client's format, named, cannot write: a 502
    /// coded `untranslatable`.
    #[error("a video status cannot be written in {0}")]
    UnwritableStatus(&'static str),
}

impl From<Error> for Fault {
    fn from(e: Error) -> Self {
        let fault = match e {
            Error::Unsupported(_) => {
                Fault::of(FaultKind::Client, e.to_string()).with(ErrorCode("unsupported".into()))
            }
            _ => Fault::new(e.to_string())
                .with(ErrorCode("untranslatable".into()))
                .with(Status(StatusCode::BAD_GATEWAY)),
        };
        fault.with_source(e)
    }
}
