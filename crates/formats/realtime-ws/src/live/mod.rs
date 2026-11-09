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

use crate::session::{self as spec, Spec, model_at};
use crate::{SessionModel, requested_model};

const NAME: &str = SPEC.name;

pub(crate) const SPEC: Spec = Spec {
    name: "openai-live",
    path: "/live/sessions",
    protocol: None,
    binary_text: false,
    transcribes: false,
};

/// OpenAI's Live API, which its full-duplex voice models (`gpt-live-1`)
/// speak: a `GET` upgrade to a path ending in `/live/sessions`, and a call
/// to it after the base URL its SDKs take (`wss://api.openai.com/v1`), with
/// no query.
///
/// Its events are the kind's where the kind has a form for them: the
/// session's start and updates (`session.start`, `session.update`,
/// answered by `session.started` and `session.updated`: its model,
/// instructions, voice and one audio format both ways), the user's audio,
/// the model's audio, and the transcripts of both, and errors. Its usage,
/// the seconds of the session billed so far, is told as
/// [`Output::Seconds`], each the seconds beyond the last; the event stays
/// native, as do the rest, such as delegation, so a relay to OpenAI keeps
/// every event.
///
/// It names its model in its start, so a session's first event is its
/// start, naming the model at `?model=` if it names none: its client routes
/// by `?model=`, as a Gemini Live client does. A call names the model
/// routing chose ([`SessionModel`](crate::SessionModel)), else the one at
/// the client's `?model=`, which routing judged, else the one its start
/// names; with none it is refused, `bad_request`.
///
/// It is full duplex: it has no turns or responses. What only another
/// format says is dropped, emulated or refused, as its README says.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenAiLive;

impl OpenAiLive {
    /// The format's name, `openai-live`, which tags its native events.
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
        let mut client = input::Client::new(model_at(io.request().uri()));
        let read = move |raw: Bytes, out: &mut Vec<Input>| client.read(raw, out);
        let mut client = output::Client::default();
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
        let wire = io.extensions().get::<Original>();
        let chosen = io.extensions().get::<SessionModel>().map(|m| m.0.clone());
        let asked = wire.and_then(requested_model);
        let (say, said) = spec::say();
        let mut provider = input::Provider::new(chosen, asked, say);
        let write = Box::new(move |event: &Input, out: &mut Vec<Bytes>| provider.write(event, out));
        let mut provider = output::Provider::default();
        let read = move |raw: Bytes, out: &mut Vec<Output>| provider.read(raw, out);
        Ok(SPEC.provider(io, None, write, Box::new(read), Some(said)))
    }
}
