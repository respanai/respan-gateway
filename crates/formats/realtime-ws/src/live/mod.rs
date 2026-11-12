//! OpenAI's Live API over WebSocket.

mod input;
mod output;
mod session;
#[cfg(test)]
mod tests;

use respan_gateway_core::Provider;
use respan_gateway_core::{Connection, Fault, Format};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::{ClientIo, ProviderIo, raw::Original};
use respan_gateway_http::{Connect, Reach, Upstream};
use respan_gateway_realtime::{Input, Output};
use respan_gateway_ws::WsTransport;

use crate::session::{self as spec, Spec};
use crate::{SessionModel, requested_model};

const NAME: &str = SPEC.name;
/// What only this format reads of a value ([`respan_gateway_core::Native`]).
enum Key {}

pub(crate) const SPEC: Spec = Spec {
    name: "openai-live",
    path: "/live/sessions",
    protocol: None,
    binary_text: false,
    transcribes: false,
};

/// OpenAI's Live API, which its full-duplex Live models (`gpt-live-1`)
/// speak: a `GET` upgrade to a path ending in `/live/sessions`, and a call
/// to it after `wss://api.openai.com/v1`, with no query.
///
/// Its session's start and updates, audio, transcripts and errors are the
/// kind's; its usage, the session's seconds so far, is [`Output::Seconds`]
/// beyond the last, beside the event; the rest stays native. Its client
/// routes by `?model=`, as a Gemini Live client does; a call names the
/// model routing chose, else that one, else the one its start names.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenAiLive;

impl OpenAiLive {
    /// The format's name, `openai-live`.
    pub const NAME: &'static str = SPEC.name;
    /// The path it is served at the end of, and sent to.
    pub const PATH: &'static str = SPEC.path;

    /// A transport to its provider at `base_url` ([`WsTransport::new`]).
    pub fn transport(&self, base_url: impl Into<String>) -> Result<WsTransport, Fault> {
        SPEC.transport(base_url)
    }
}

impl Format<ClientIo, Input, Output> for OpenAiLive {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn matches(&self, io: &ClientIo) -> bool {
        SPEC.matches(io)
    }

    fn bind(&self, io: ClientIo) -> Result<Connection<Input, Output>, Fault> {
        let mut client = input::Client::new();
        let read = move |raw: Bytes, out: &mut Vec<Input>| client.read(raw, out);
        let mut client = output::Client::new();
        let write = Box::new(move |event: &Output, out: &mut Vec<Bytes>| client.write(event, out));
        Ok(SPEC.client(io, Box::new(read), write))
    }
}

/// Its spec's: a bearer key, as a WebSocket session.
impl Upstream for OpenAiLive {
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

impl Format<ProviderIo, Output, Input> for OpenAiLive {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        let chosen = io.extensions().get::<SessionModel>().map(|m| m.0.clone());
        let wire = io.extensions().get::<Original>();
        let model = chosen.or_else(|| requested_model(wire?));
        let (say, said) = spec::say();
        let mut provider = input::Provider::new(model, say);
        let write = Box::new(move |event: &Input, out: &mut Vec<Bytes>| provider.write(event, out));
        let mut provider = output::Provider::new();
        let read = move |raw: Bytes, out: &mut Vec<Output>| provider.read(raw, out);
        Ok(SPEC.provider(io, None, write, Box::new(read), Some(said)))
    }
}
