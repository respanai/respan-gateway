//! OpenAI's Responses over WebSocket.

use respan_gateway_core::Provider;
use respan_gateway_core::{Connection, Fault, Format};
use respan_gateway_http::{ClientIo, ProviderIo};
use respan_gateway_http::{Connect, Reach, Upstream};
use respan_gateway_realtime::{Input, Output};
use respan_gateway_ws::WsTransport;

use crate::session::{self, Spec};

pub(crate) const SPEC: Spec = Spec {
    name: "responses-ws",
    path: "/responses",
    protocol: None,
    binary_text: false,
    transcribes: false,
    harmless: None,
};

/// OpenAI's Responses over WebSocket: a `GET` upgrade to a path ending in
/// `/responses`, as for HTTP, and a call to it after the base URL its SDKs
/// take (`wss://api.openai.com/v1`). Only an upgrade is this format's: a
/// `POST` there is no session.
///
/// It is not realtime: it streams Responses over a socket, many requests
/// and replies at once. Every event is native, so it translates to no other
/// format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResponsesWs;

impl ResponsesWs {
    /// The format's name, `responses-ws`, which tags its native events.
    pub const NAME: &'static str = SPEC.name;
    /// The path it is served at the end of, and sent to.
    pub const PATH: &'static str = SPEC.path;

    /// A transport to its provider at `base_url` ([`WsTransport::new`]).
    pub fn transport(&self, base_url: impl Into<String>) -> Result<WsTransport, Fault> {
        SPEC.transport(base_url)
    }
}

impl Format<ClientIo, Input, Output> for ResponsesWs {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn matches(&self, io: &ClientIo) -> bool {
        SPEC.matches(io)
    }

    fn bind(&self, io: ClientIo) -> Result<Connection<Input, Output>, Fault> {
        let (read, write) = session::native_client(Self::NAME);
        Ok(SPEC.client(io, read, write))
    }
}

/// Its spec's: a bearer key, as a WebSocket session.
impl Upstream for ResponsesWs {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn reach(&self) -> Reach {
        Reach::Session {
            binary_text: SPEC.binary_text,
        }
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for ResponsesWs {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        let (write, read) = session::native_provider(Self::NAME);
        Ok(SPEC.provider(io, None, write, read, None))
    }
}
