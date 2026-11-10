use respan_gateway_core::Fault;

use crate::ProviderClose;

/// What ends a WebSocket session, a [`Fault`] by `?` or `Fault::from`,
/// which keeps it as its source ([`Fault::cause`]).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The provider closed with a code other than 1000: the provider's, with
    /// its code as a [`ProviderClose`].
    #[error("the provider closed the session with {code}{}", reason_of(.reason))]
    ProviderClosed {
        /// The Close's code.
        code: u16,
        /// The Close's reason, empty if it gave none.
        reason: String,
    },
}

fn reason_of(reason: &str) -> String {
    match reason.is_empty() {
        true => String::new(),
        false => format!(": {reason}"),
    }
}

impl From<Error> for Fault {
    fn from(e: Error) -> Self {
        let Error::ProviderClosed { code, .. } = e;
        Fault::upstream(e.to_string())
            .with(ProviderClose(code))
            .with_source(e)
    }
}
