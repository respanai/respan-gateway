//! Mistral's realtime transcription over WebSocket.

mod input;
mod output;
mod session;
#[cfg(test)]
mod tests;

use respan_gateway_core::Provider;
use respan_gateway_core::{Connection, Fault, Format};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::raw::Original;
use respan_gateway_http::{ClientIo, ProviderIo};
use respan_gateway_http::{Connect, Reach, Upstream};
use respan_gateway_realtime::{Input, Output};
use respan_gateway_ws::WsTransport;

use crate::foreign::{Drop, Harmless};
use crate::session::{Spec, model_at, model_query};
use crate::{SessionModel, requested_model};

const NAME: &str = SPEC.name;

/// What of Mistral's own natives another format may drop: its session's
/// streaming delay, which says how soon text comes, not what it says.
pub(crate) const HARMLESS: Harmless = Harmless {
    session: &["session.target_streaming_delay_ms"],
    vad: &[],
    message: &[],
    event: &[],
    input: |event| {
        (
            Drop::Never,
            event["type"].as_str().unwrap_or_default().to_owned(),
        )
    },
    said: |_| false,
};

pub(crate) const SPEC: Spec = Spec {
    name: "mistral-realtime",
    path: "/audio/transcriptions/realtime",
    protocol: None,
    binary_text: false,
    transcribes: true,
    harmless: Some(&HARMLESS),
};

/// Mistral's realtime transcription: a `GET` upgrade to a path ending in
/// `/audio/transcriptions/realtime`, and a call to it after the base URL its
/// API takes (`wss://api.mistral.ai/v1`), the model at `?model=`, the key a
/// bearer token.
///
/// Its events are the kind's where the kind has a form for them: the
/// session and its audio format, the user's audio, a flush as a commit, the
/// transcript's deltas and its end, with the usage the end reports, and
/// errors. What the kind has no form for, such as a detected language or a
/// segment, stays native. It transcribes and says nothing else: a message,
/// a tool's answer, a response to make or cancel, and a session that asks
/// for speech, instructions or tools are refused. The end of the client's
/// events tells it the audio ended.
///
/// A call dials the model routing chose ([`SessionModel`](crate::SessionModel)),
/// else the one at the client's `?model=`; with neither it is refused,
/// `bad_request`.
///
/// Its client's session is a transcription from its connect, its turns the
/// client's: a flush, or the end of its audio, makes the transcript whole.
/// It is told the user's transcript, the session and errors, and what the
/// model says nothing of.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MistralRealtime;

impl MistralRealtime {
    /// The format's name, `mistral-realtime`, which tags its native events.
    pub const NAME: &'static str = SPEC.name;
    /// The path it is sent to.
    pub const PATH: &'static str = SPEC.path;

    /// A transport to its provider at `base_url` ([`WsTransport::new`]).
    pub fn transport(&self, base_url: impl Into<String>) -> Result<WsTransport, Fault> {
        SPEC.transport(base_url)
    }
}

/// Its spec's: a bearer key, as a WebSocket session.
impl Upstream for MistralRealtime {
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

impl Format<ClientIo, Input, Output> for MistralRealtime {
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

impl Format<ProviderIo, Output, Input> for MistralRealtime {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        // The model routing chose, else the one the client's handshake asks for.
        let chosen = io.extensions().get::<SessionModel>().map(|m| m.0.clone());
        let asked = || requested_model(io.extensions().get::<Original>()?);
        let model = chosen.or_else(asked).ok_or_else(|| {
            Fault::bad_request(
                "Mistral realtime takes its model at connect: name it with ?model= on the session's URL",
            )
        })?;
        let query = Some(model_query(&model));
        let write = Box::new(input::Provider::new(model));
        let mut provider = output::Provider;
        let read = move |raw: Bytes, out: &mut Vec<Output>| provider.read(raw, out);
        Ok(SPEC.provider(io, query, write, Box::new(read), None))
    }
}
